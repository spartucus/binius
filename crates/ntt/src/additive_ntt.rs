// Copyright 2024 Ulvetanna Inc.

use binius_field::{
	unpack_scalars_mut, BinaryField, ExtensionField, Field, PackedExtensionField, PackedField,
};
use p3_util::log2_strict_usize;

use super::error::Error;

/// The additive NTT defined defined in [LCH14].
///
/// [LCH14]: <https://arxiv.org/abs/1404.3458>
pub trait AdditiveNTT<P: PackedField> {
	/// Forward transformation defined in [LCH14] on a batch of inputs.
	///
	/// Input is the vector of polynomial coefficients in novel basis, output is in Lagrange basis.
	/// The batched inputs are interleaved, which improves the cache-efficiency of the computation.
	///
	/// [LCH14]: <https://arxiv.org/abs/1404.3458>
	fn forward_transform(
		&self,
		data: &mut [P],
		coset: u32,
		log_batch_size: usize,
	) -> Result<(), Error>;

	/// Inverse transformation defined in [LCH14] on a batch of inputs.
	///
	/// Input is the vector of polynomial coefficients in Lagrange basis, output is in novel basis.
	/// The batched inputs are interleaved, which improves the cache-efficiency of the computation.
	///
	/// [LCH14]: https://arxiv.org/abs/1404.3458
	fn inverse_transform(
		&self,
		data: &mut [P],
		coset: u32,
		log_batch_size: usize,
	) -> Result<(), Error>;

	fn forward_transform_ext<PE>(&self, data: &mut [PE], coset: u32) -> Result<(), Error>
	where
		PE: PackedExtensionField<P>,
		PE::Scalar: ExtensionField<P::Scalar>,
	{
		if !PE::Scalar::DEGREE.is_power_of_two() {
			return Err(Error::PowerOfTwoExtensionDegreeRequired);
		}

		let log_batch_size = log2_strict_usize(PE::Scalar::DEGREE);
		self.forward_transform(PE::cast_to_bases_mut(data), coset, log_batch_size)
	}

	fn inverse_transform_ext<PE>(&self, data: &mut [PE], coset: u32) -> Result<(), Error>
	where
		PE: PackedExtensionField<P>,
		PE::Scalar: ExtensionField<P::Scalar>,
	{
		if !PE::Scalar::DEGREE.is_power_of_two() {
			return Err(Error::PowerOfTwoExtensionDegreeRequired);
		}

		let log_batch_size = log2_strict_usize(PE::Scalar::DEGREE);
		self.inverse_transform(PE::cast_to_bases_mut(data), coset, log_batch_size)
	}
}

/// Implementation of `AdditiveNTT` that does on-the-fly computation to reduce its memory footprint.
///
/// This implementation uses a small amount of precomputed constants from which the twiddle factors
/// are derived on the fly (OTF). The number of constants is ~1/2 k^2 field elements for a domain
/// of size 2^k.
#[derive(Debug)]
pub struct AdditiveNTTWithOTFCompute<F: BinaryField> {
	log_domain_size: usize,
	s_evals: Vec<Vec<F>>,
}

impl<F: BinaryField> AdditiveNTTWithOTFCompute<F> {
	pub fn new(log_domain_size: usize) -> Result<Self, Error> {
		let s_evals = precompute_subspace_evals(log_domain_size)?;
		Ok(AdditiveNTTWithOTFCompute {
			log_domain_size,
			s_evals,
		})
	}

	/// Calculate twiddle within sub-block
	fn calculate_twiddle<P: PackedField<Scalar = F>>(
		&self,
		s_evals_i: &[F],
		log_blocks_count: usize,
		log_block_len: usize,
		initial_twiddle: F,
	) -> P {
		let mut twiddle = P::default();
		for k in 0..1 << log_blocks_count {
			let subblock_twiddle_0 = subset_sum(s_evals_i, log_blocks_count, k);
			let subblock_twiddle_1 = subblock_twiddle_0 + s_evals_i[log_blocks_count];
			for l in 0..1 << (log_block_len) {
				let idx0 = k << (log_block_len + 1) | l;
				let idx1 = idx0 | 1 << log_block_len;
				twiddle.set(idx0, initial_twiddle + subblock_twiddle_0);
				twiddle.set(idx1, initial_twiddle + subblock_twiddle_1);
			}
		}

		twiddle
	}

	/// Get the normalized subspace polynomial evaluation $\hat{W}_i(\beta_j)$.
	///
	/// ## Preconditions
	///
	/// * `i` must be less than `self.log_domain_size()`
	/// * `j` must be less than `i`
	pub fn get_subspace_eval(&self, i: usize, j: usize) -> F {
		self.s_evals[i][j]
	}
}

impl<F: BinaryField, P> AdditiveNTT<P> for AdditiveNTTWithOTFCompute<F>
where
	P: PackedField<Scalar = F> + PackedExtensionField<F>,
{
	fn forward_transform(
		&self,
		data: &mut [P],
		coset: u32,
		log_batch_size: usize,
	) -> Result<(), Error> {
		match data.len() {
			0 => return Ok(()),
			1 => {
				return match P::WIDTH {
					1 => Ok(()),
					// Special case when data.len() is 1 and packing width > 1. We must unpack the data
					// in order to be able to interleave below.
					_ => self.forward_transform(unpack_scalars_mut(data), coset, log_batch_size),
				};
			}
			_ => {}
		};

		let log_b = log_batch_size;

		let NTTParams {
			log_n,
			log_w,
			coset_bits,
			..
		} = check_batch_transform_inputs(self.log_domain_size, data, coset, log_b)?;

		// Cutoff is the stage of the NTT where each the butterfly units are contained within
		// packed base field elements.
		let cutoff = log_w.saturating_sub(log_b);

		for i in (cutoff..log_n).rev() {
			let s_evals_i = &self.s_evals[i];
			let coset_twiddle = subset_sum(&s_evals_i[log_n - 1 - i..], coset_bits, coset as usize);

			for j in 0..1 << (log_n - 1 - i) {
				let block_twiddle = subset_sum(s_evals_i, log_n - 1 - i, j);
				let twiddle = block_twiddle + coset_twiddle;

				for k in 0..1 << (i + log_b - log_w) {
					let idx0 = j << (i + log_b - log_w + 1) | k;
					let idx1 = idx0 | 1 << (i + log_b - log_w);
					data[idx0] += data[idx1] * twiddle;
					data[idx1] += data[idx0];
				}
			}
		}

		for i in (0..cutoff).rev() {
			let s_evals_i = &self.s_evals[i];
			let coset_twiddle = subset_sum(&s_evals_i[log_n - 1 - i..], coset_bits, coset as usize);

			let log_block_len = i + log_b;
			let log_blocks_count = cutoff - i - 1;
			for j in 0..1 << (log_n - 1 - cutoff) {
				let block_twiddle = subset_sum(&s_evals_i[cutoff - i..], log_n - 1 - cutoff, j);

				let twiddle: P = self.calculate_twiddle(
					s_evals_i,
					log_blocks_count,
					log_block_len,
					block_twiddle + coset_twiddle,
				);

				let (mut u, mut v) = data[j << 1].interleave(data[j << 1 | 1], log_block_len);
				u += v * twiddle;
				v += u;
				(data[j << 1], data[j << 1 | 1]) = u.interleave(v, log_block_len);
			}
		}

		Ok(())
	}

	fn inverse_transform(
		&self,
		data: &mut [P],
		coset: u32,
		log_batch_size: usize,
	) -> Result<(), Error> {
		match data.len() {
			0 => return Ok(()),
			1 => {
				return match P::WIDTH {
					1 => Ok(()),
					_ => self.inverse_transform(unpack_scalars_mut(data), coset, log_batch_size),
				};
			}
			_ => {}
		};

		let log_b = log_batch_size;

		let NTTParams {
			log_n,
			log_w,
			coset_bits,
		} = check_batch_transform_inputs(self.log_domain_size, data, coset, log_b)?;

		// Cutoff is the stage of the NTT where each the butterfly units are contained within
		// packed base field elements.
		let cutoff = log_w.saturating_sub(log_b);

		for i in 0..cutoff {
			let s_evals_i = &self.s_evals[i];
			let coset_twiddle = subset_sum(&s_evals_i[log_n - 1 - i..], coset_bits, coset as usize);

			let log_block_len = i + log_b;
			let log_blocks_count = cutoff - i - 1;
			for j in 0..1 << (log_n - 1 - cutoff) {
				let block_twiddle = subset_sum(&s_evals_i[cutoff - i..], log_n - 1 - cutoff, j);

				let twiddle: P = self.calculate_twiddle(
					s_evals_i,
					log_blocks_count,
					log_block_len,
					block_twiddle + coset_twiddle,
				);

				let (mut u, mut v) = data[j << 1].interleave(data[j << 1 | 1], log_block_len);
				v += u;
				u += v * twiddle;
				(data[j << 1], data[j << 1 | 1]) = u.interleave(v, log_block_len);
			}
		}

		for i in cutoff..log_n {
			let s_evals_i = &self.s_evals[i];
			let coset_twiddle = subset_sum(&s_evals_i[log_n - 1 - i..], coset_bits, coset as usize);

			for j in 0..1 << (log_n - 1 - i) {
				let block_twiddle = subset_sum(s_evals_i, log_n - 1 - i, j);
				let twiddle = block_twiddle + coset_twiddle;

				for k in 0..1 << (i + log_b - log_w) {
					let idx0 = j << (i + log_b - log_w + 1) | k;
					let idx1 = idx0 | 1 << (i + log_b - log_w);
					data[idx1] += data[idx0];
					data[idx0] += data[idx1] * twiddle;
				}
			}
		}

		Ok(())
	}
}

fn subset_sum<F: Field>(values: &[F], n_bits: usize, index: usize) -> F {
	(0..n_bits)
		.filter(|b| (index >> b) & 1 != 0)
		.map(|b| values[b])
		.sum()
}

/// The additive NTT defined defined in [LCH14] with a larger table of precomputed constants.
///
/// This implementation precomputes all 2^k twiddle factors for a domain of size 2^k.
///
/// [LCH14]: <https://arxiv.org/abs/1404.3458>
#[derive(Debug)]
pub struct AdditiveNTTWithPrecompute<F: BinaryField> {
	log_domain_size: usize,
	s_evals_expanded: Vec<Vec<F>>,
}

impl<F: BinaryField> AdditiveNTTWithPrecompute<F> {
	pub fn new(log_domain_size: usize) -> Result<Self, Error> {
		let s_evals = precompute_subspace_evals::<F>(log_domain_size)?;
		let s_evals_expanded = s_evals
			.iter()
			.map(|s_evals_i| {
				let mut expanded = Vec::with_capacity(1 << s_evals_i.len());
				expanded.push(F::ZERO);
				for &eval in s_evals_i.iter() {
					for i in 0..expanded.len() {
						expanded.push(expanded[i] + eval);
					}
				}
				expanded
			})
			.collect::<Vec<_>>();

		Ok(AdditiveNTTWithPrecompute {
			log_domain_size,
			s_evals_expanded,
		})
	}

	#[inline]
	fn calculate_twiddle<P: PackedField<Scalar = F>>(
		&self,
		s_evals_i: &[F],
		log_blocks_count: usize,
		log_block_len: usize,
		evals_index_init_val: usize,
	) -> P {
		let mut twiddle = P::default();
		for k in 0..1 << log_blocks_count {
			let subblock_twiddle_0 = s_evals_i[evals_index_init_val | k];
			let subblock_twiddle_1 = s_evals_i[evals_index_init_val | 1 << log_blocks_count | k];
			for l in 0..1 << log_block_len {
				let idx0 = k << (log_block_len + 1) | l;
				let idx1 = idx0 | 1 << log_block_len;
				twiddle.set(idx0, subblock_twiddle_0);
				twiddle.set(idx1, subblock_twiddle_1);
			}
		}

		twiddle
	}

	/// Get the normalized subspace polynomial evaluation $\hat{W}_i(\beta_j)$.
	///
	/// ## Preconditions
	///
	/// * `i` must be less than `self.log_domain_size()`
	/// * `j` must be less than `i`
	pub fn get_subspace_eval(&self, i: usize, j: usize) -> F {
		self.s_evals_expanded[i][j]
	}
}

impl<F: BinaryField, P> AdditiveNTT<P> for AdditiveNTTWithPrecompute<F>
where
	P: PackedField<Scalar = F> + PackedExtensionField<F>,
{
	fn forward_transform(
		&self,
		data: &mut [P],
		coset: u32,
		log_batch_size: usize,
	) -> Result<(), Error> {
		match data.len() {
			0 => return Ok(()),
			1 => {
				return match P::WIDTH {
					1 => Ok(()),
					_ => self.forward_transform(unpack_scalars_mut(data), coset, log_batch_size),
				};
			}
			_ => {}
		};

		let log_b = log_batch_size;

		let NTTParams { log_n, log_w, .. } =
			check_batch_transform_inputs(self.log_domain_size, data, coset, log_b)?;

		// Cutoff is the stage of the NTT where each the butterfly units are contained within
		// packed base field elements.
		let cutoff = log_w.saturating_sub(log_b);

		for i in (cutoff..log_n).rev() {
			let s_evals_i = &self.s_evals_expanded[i];
			for j in 0..1 << (log_n - 1 - i) {
				let twiddle = s_evals_i[(coset as usize) << (log_n - 1 - i) | j];
				for k in 0..1 << (i + log_b - log_w) {
					let idx0 = j << (i + log_b - log_w + 1) | k;
					let idx1 = idx0 | 1 << (i + log_b - log_w);
					data[idx0] += data[idx1] * twiddle;
					data[idx1] += data[idx0];
				}
			}
		}

		for i in (0..cutoff).rev() {
			let s_evals_i = &self.s_evals_expanded[i];
			let log_blocks_count = cutoff - i - 1;
			let log_block_len = i + log_b;
			for j in 0..1 << (log_n - 1 - cutoff) {
				let evals_index_init_val = (coset as usize) << (log_n - 1 - i) | j << (cutoff - i);
				let twiddle: P = self.calculate_twiddle(
					s_evals_i,
					log_blocks_count,
					log_block_len,
					evals_index_init_val,
				);

				let (mut u, mut v) = data[j << 1].interleave(data[j << 1 | 1], log_block_len);
				u += v * twiddle;
				v += u;
				(data[j << 1], data[j << 1 | 1]) = u.interleave(v, log_block_len);
			}
		}

		Ok(())
	}

	fn inverse_transform(
		&self,
		data: &mut [P],
		coset: u32,
		log_batch_size: usize,
	) -> Result<(), Error> {
		match data.len() {
			0 => return Ok(()),
			1 => {
				return match P::WIDTH {
					1 => Ok(()),
					_ => self.inverse_transform(unpack_scalars_mut(data), coset, log_batch_size),
				};
			}
			_ => {}
		};

		let log_b = log_batch_size;

		let NTTParams { log_n, log_w, .. } =
			check_batch_transform_inputs(self.log_domain_size, data, coset, log_b)?;

		// Cutoff is the stage of the NTT where each the butterfly units are contained within
		// packed base field elements.
		let cutoff = log_w.saturating_sub(log_b);

		for i in 0..cutoff {
			let s_evals_i = &self.s_evals_expanded[i];

			let log_block_count = cutoff - i - 1;
			let log_block_len = i + log_b;
			for j in 0..1 << (log_n - 1 - cutoff) {
				let evals_index_init_val = (coset as usize) << (log_n - 1 - i) | j << (cutoff - i);
				let twiddle: P = self.calculate_twiddle(
					s_evals_i,
					log_block_count,
					log_block_len,
					evals_index_init_val,
				);

				let (mut u, mut v) = data[j << 1].interleave(data[j << 1 | 1], log_block_len);
				v += u;
				u += v * twiddle;
				(data[j << 1], data[j << 1 | 1]) = u.interleave(v, log_block_len);
			}
		}

		for i in cutoff..log_n {
			let s_evals_i = &self.s_evals_expanded[i];
			for j in 0..1 << (log_n - 1 - i) {
				let twiddle = s_evals_i[(coset as usize) << (log_n - 1 - i) | j];
				for k in 0..1 << (i + log_b - log_w) {
					let idx0 = j << (i + log_b - log_w + 1) | k;
					let idx1 = idx0 | 1 << (i + log_b - log_w);
					data[idx1] += data[idx0];
					data[idx0] += data[idx1] * twiddle;
				}
			}
		}

		Ok(())
	}
}

fn precompute_subspace_evals<F: BinaryField>(log_domain_size: usize) -> Result<Vec<Vec<F>>, Error> {
	if F::N_BITS < log_domain_size {
		return Err(Error::FieldTooSmall { log_domain_size });
	}

	let mut s_evals = Vec::with_capacity(log_domain_size);

	// normalization_consts[i] = W_i(2^i)
	let mut normalization_consts = Vec::with_capacity(log_domain_size);
	normalization_consts.push(F::ONE);

	let s0_evals = (1..log_domain_size)
		.map(|i| F::basis(i).expect("basis vector must exist because of FieldTooSmall check above"))
		.collect::<Vec<_>>();
	s_evals.push(s0_evals);

	for _ in 1..log_domain_size {
		let (norm_const_i, s_i_evals) = {
			let norm_prev = *normalization_consts
				.last()
				.expect("normalization_consts is not empty");
			let s_prev_evals = s_evals.last().expect("s_evals is not empty");

			let norm_const_i = subspace_map(s_prev_evals[0], norm_prev);
			let s_i_evals = s_prev_evals
				.iter()
				.skip(1)
				.map(|&s_ij_prev| subspace_map(s_ij_prev, norm_prev))
				.collect::<Vec<_>>();

			(norm_const_i, s_i_evals)
		};

		normalization_consts.push(norm_const_i);
		s_evals.push(s_i_evals);
	}

	for (norm_const_i, s_evals_i) in normalization_consts.iter().zip(s_evals.iter_mut()) {
		let inv_norm_const = norm_const_i
			.invert()
			.expect("normalization constants are nonzero");
		for s_ij in s_evals_i.iter_mut() {
			*s_ij *= inv_norm_const;
		}
	}

	Ok(s_evals)
}

fn subspace_map<F: Field>(elem: F, constant: F) -> F {
	elem.square() + constant * elem
}

struct NTTParams {
	log_n: usize,
	log_w: usize,
	coset_bits: usize,
}

fn check_batch_transform_inputs<PB: PackedField>(
	log_domain_size: usize,
	data: &[PB],
	coset: u32,
	log_batch_size: usize,
) -> Result<NTTParams, Error> {
	if !data.len().is_power_of_two() {
		return Err(Error::PowerOfTwoLengthRequired);
	}
	if !PB::WIDTH.is_power_of_two() {
		return Err(Error::PackingWidthMustDivideDimension);
	}

	let n = (data.len() * PB::WIDTH) >> log_batch_size;
	if n == 0 {
		return Err(Error::BatchTooLarge);
	}

	let log_n = n.trailing_zeros() as usize;
	let log_w = PB::WIDTH.trailing_zeros() as usize;

	let coset_bits = 32 - coset.leading_zeros() as usize;
	if log_n + coset_bits > log_domain_size {
		return Err(Error::DomainTooSmall {
			log_required_domain_size: log_n + coset_bits,
		});
	}

	Ok(NTTParams {
		log_n,
		log_w,
		coset_bits,
	})
}

#[cfg(test)]
mod tests {
	use super::*;
	use assert_matches::assert_matches;
	use binius_field::{
		packed_binary_field::{PackedBinaryField16x8b, PackedBinaryField4x32b},
		BinaryField32b, BinaryField8b, PackedBinaryField8x16b,
	};
	use rand::{rngs::StdRng, SeedableRng};
	use std::{fmt::Debug, iter::repeat_with};

	trait SimpleAdditiveNTT<F: BinaryField> {
		fn forward_transform_simple<FF>(&self, data: &mut [FF], coset: u32) -> Result<(), Error>
		where
			FF: ExtensionField<F>;

		fn inverse_transform_simple<FF>(&self, data: &mut [FF], coset: u32) -> Result<(), Error>
		where
			FF: ExtensionField<F>;
	}

	impl<F: BinaryField> SimpleAdditiveNTT<F> for AdditiveNTTWithOTFCompute<F> {
		/// Simple implementation of forward transform on non-packed field elements for testing.
		fn forward_transform_simple<FF>(&self, data: &mut [FF], coset: u32) -> Result<(), Error>
		where
			FF: ExtensionField<F>,
		{
			let n = data.len();
			assert!(n.is_power_of_two());

			let log_n = n.trailing_zeros() as usize;
			let coset_bits = 32 - coset.leading_zeros() as usize;
			if log_n + coset_bits > self.log_domain_size {
				return Err(Error::DomainTooSmall {
					log_required_domain_size: log_n + coset_bits,
				});
			}

			for i in (0..log_n).rev() {
				let s_evals_i = &self.s_evals[i];
				let coset_twiddle =
					subset_sum(&s_evals_i[log_n - 1 - i..], coset_bits, coset as usize);

				for j in 0..1 << (log_n - 1 - i) {
					let block_twiddle = subset_sum(s_evals_i, log_n - 1 - i, j);
					let twiddle = block_twiddle + coset_twiddle;

					for k in 0..1 << i {
						let idx0 = j << (i + 1) | k;
						let idx1 = idx0 | 1 << i;
						data[idx0] += data[idx1] * twiddle;
						data[idx1] += data[idx0];
					}
				}
			}

			Ok(())
		}

		/// Simple implementation of inverse transform on non-packed field elements for testing.
		fn inverse_transform_simple<FF>(&self, data: &mut [FF], coset: u32) -> Result<(), Error>
		where
			FF: ExtensionField<F>,
		{
			let n = data.len();
			assert!(n.is_power_of_two());

			let log_n = n.trailing_zeros() as usize;
			let coset_bits = 32 - coset.leading_zeros() as usize;
			if log_n + coset_bits > self.log_domain_size {
				return Err(Error::DomainTooSmall {
					log_required_domain_size: log_n + coset_bits,
				});
			}

			for i in 0..log_n {
				let s_evals_i = &self.s_evals[i];
				let coset_twiddle =
					subset_sum(&s_evals_i[log_n - 1 - i..], coset_bits, coset as usize);

				for j in 0..1 << (log_n - 1 - i) {
					let block_twiddle = subset_sum(s_evals_i, log_n - 1 - i, j);
					let twiddle = block_twiddle + coset_twiddle;

					for k in 0..1 << i {
						let idx0 = j << (i + 1) | k;
						let idx1 = idx0 | 1 << i;
						data[idx1] += data[idx0];
						data[idx0] += data[idx1] * twiddle;
					}
				}
			}

			Ok(())
		}
	}

	impl<F: BinaryField> SimpleAdditiveNTT<F> for AdditiveNTTWithPrecompute<F> {
		/// Simple implementation of forward transform on non-packed field elements for testing.
		fn forward_transform_simple<FF>(&self, data: &mut [FF], coset: u32) -> Result<(), Error>
		where
			FF: ExtensionField<F>,
		{
			let n = data.len();
			assert!(n.is_power_of_two());

			let log_n = n.trailing_zeros() as usize;
			let coset_bits = 32 - coset.leading_zeros() as usize;
			if log_n + coset_bits > self.log_domain_size {
				return Err(Error::DomainTooSmall {
					log_required_domain_size: log_n + coset_bits,
				});
			}

			for i in (0..log_n).rev() {
				let s_evals_i = &self.s_evals_expanded[i];
				for j in 0..1 << (log_n - 1 - i) {
					let twiddle = s_evals_i[(coset as usize) << (log_n - 1 - i) | j];
					for k in 0..1 << i {
						let idx0 = j << (i + 1) | k;
						let idx1 = idx0 | 1 << i;
						data[idx0] += data[idx1] * twiddle;
						data[idx1] += data[idx0];
					}
				}
			}

			Ok(())
		}

		/// Simple implementation of inverse transform on non-packed field elements for testing.
		fn inverse_transform_simple<FF>(&self, data: &mut [FF], coset: u32) -> Result<(), Error>
		where
			FF: ExtensionField<F>,
		{
			let n = data.len();
			assert!(n.is_power_of_two());

			let log_n = n.trailing_zeros() as usize;
			let coset_bits = 32 - coset.leading_zeros() as usize;
			if log_n + coset_bits > self.log_domain_size {
				return Err(Error::DomainTooSmall {
					log_required_domain_size: log_n + coset_bits,
				});
			}

			for i in 0..log_n {
				let s_evals_i = &self.s_evals_expanded[i];
				for j in 0..1 << (log_n - 1 - i) {
					let twiddle = s_evals_i[(coset as usize) << (log_n - 1 - i) | j];
					for k in 0..1 << i {
						let idx0 = j << (i + 1) | k;
						let idx1 = idx0 | 1 << i;
						data[idx1] += data[idx0];
						data[idx0] += data[idx1] * twiddle;
					}
				}
			}

			Ok(())
		}
	}

	#[test]
	fn test_additive_ntt_fails_with_field_too_small() {
		assert_matches!(
			<AdditiveNTTWithOTFCompute<BinaryField8b>>::new(10),
			Err(Error::FieldTooSmall {
				log_domain_size: 10
			})
		);
	}

	#[test]
	fn test_additive_ntt_transform() {
		let ntt = <AdditiveNTTWithOTFCompute<BinaryField8b>>::new(8).unwrap();
		let data = (0..1 << 6)
			.map(|i| BinaryField8b::new(i as u8))
			.collect::<Vec<_>>();

		let mut result = data.clone();
		for coset in 0..4 {
			ntt.inverse_transform_simple(&mut result, coset).unwrap();
			ntt.forward_transform_simple(&mut result, coset).unwrap();
			assert_eq!(result, data);
		}
	}

	#[test]
	fn test_additive_ntt_with_precompute_matches() {
		let ntt = <AdditiveNTTWithOTFCompute<BinaryField8b>>::new(8).unwrap();
		let ntt_with_precompute = <AdditiveNTTWithPrecompute<BinaryField8b>>::new(8).unwrap();
		let data = (0..1 << 6)
			.map(|i| BinaryField8b::new(i as u8))
			.collect::<Vec<_>>();

		let mut result1 = data.clone();
		let mut result2 = data;
		for coset in 0..4 {
			ntt.inverse_transform_simple(&mut result1, coset).unwrap();
			ntt_with_precompute
				.inverse_transform_simple(&mut result2, coset)
				.unwrap();
			assert_eq!(result1, result2);

			ntt.forward_transform_simple(&mut result1, coset).unwrap();
			ntt_with_precompute
				.forward_transform_simple(&mut result2, coset)
				.unwrap();
			assert_eq!(result1, result2);
		}
	}

	#[test]
	fn test_additive_ntt_transform_over_larger_field() {
		let mut rng = StdRng::seed_from_u64(0);

		let ntt = <AdditiveNTTWithOTFCompute<BinaryField8b>>::new(8).unwrap();
		let data = repeat_with(|| <BinaryField32b as Field>::random(&mut rng))
			.take(1 << 6)
			.collect::<Vec<_>>();

		let mut result = data.clone();
		for coset in 0..4 {
			ntt.inverse_transform_simple(&mut result, coset).unwrap();
			ntt.forward_transform_simple(&mut result, coset).unwrap();
			assert_eq!(result, data);
		}
	}

	#[test]
	fn test_packed_ntt_on_scalars() {
		type Packed = PackedBinaryField16x8b;

		let mut rng = StdRng::seed_from_u64(0);

		let ntt = <AdditiveNTTWithOTFCompute<BinaryField8b>>::new(8).unwrap();
		let mut data = repeat_with(|| Packed::random(&mut rng))
			.take(1 << 2)
			.collect::<Vec<_>>();
		let mut data_copy = data.clone();

		ntt.inverse_transform_simple::<BinaryField8b>(unpack_scalars_mut(&mut data), 2)
			.unwrap();
		AdditiveNTT::<Packed>::inverse_transform_ext(&ntt, &mut data_copy, 2).unwrap();
		assert_eq!(data, data_copy);

		ntt.forward_transform_simple::<BinaryField8b>(unpack_scalars_mut(&mut data), 3)
			.unwrap();
		AdditiveNTT::<Packed>::forward_transform_ext(&ntt, &mut data_copy, 3).unwrap();
		assert_eq!(data, data_copy);
	}

	fn check_packed_ntt_on_scalars_with_message_size_one<P, S, NTT>(ntt: NTT)
	where
		S: Field,
		P: PackedField<Scalar = S>,
		P::Scalar: BinaryField + ExtensionField<S>,
		P: PackedExtensionField<S> + Debug,
		NTT: AdditiveNTT<P> + SimpleAdditiveNTT<P::Scalar>,
	{
		let mut rng = StdRng::seed_from_u64(0);

		let mut data = repeat_with(|| <P as PackedField>::random(&mut rng))
			.take(1 << 0)
			.collect::<Vec<_>>();
		let mut data_copy = data.clone();

		ntt.inverse_transform_simple::<P::Scalar>(unpack_scalars_mut(&mut data), 2)
			.unwrap();
		AdditiveNTT::<P>::inverse_transform_ext(&ntt, &mut data_copy, 2).unwrap();
		assert_eq!(data, data_copy);

		ntt.forward_transform_simple::<P::Scalar>(unpack_scalars_mut(&mut data), 3)
			.unwrap();
		AdditiveNTT::<P>::forward_transform_ext(&ntt, &mut data_copy, 3).unwrap();
		assert_eq!(data, data_copy);
	}

	#[test]
	fn test_packed_ntt_with_otf_compute_on_scalars_with_message_size_one() {
		check_packed_ntt_on_scalars_with_message_size_one::<PackedBinaryField4x32b, _, _>(
			AdditiveNTTWithOTFCompute::new(8).unwrap(),
		);
		check_packed_ntt_on_scalars_with_message_size_one::<PackedBinaryField8x16b, _, _>(
			AdditiveNTTWithOTFCompute::new(8).unwrap(),
		);
		check_packed_ntt_on_scalars_with_message_size_one::<PackedBinaryField16x8b, _, _>(
			AdditiveNTTWithOTFCompute::new(8).unwrap(),
		);
		check_packed_ntt_on_scalars_with_message_size_one::<BinaryField32b, _, _>(
			AdditiveNTTWithOTFCompute::new(8).unwrap(),
		);
	}

	#[test]
	fn test_packed_ntt_with_precompute_on_scalars_with_message_size_one() {
		check_packed_ntt_on_scalars_with_message_size_one::<PackedBinaryField4x32b, _, _>(
			AdditiveNTTWithPrecompute::new(8).unwrap(),
		);
		check_packed_ntt_on_scalars_with_message_size_one::<PackedBinaryField8x16b, _, _>(
			AdditiveNTTWithPrecompute::new(8).unwrap(),
		);
		check_packed_ntt_on_scalars_with_message_size_one::<PackedBinaryField16x8b, _, _>(
			AdditiveNTTWithPrecompute::new(8).unwrap(),
		);
		check_packed_ntt_on_scalars_with_message_size_one::<BinaryField32b, _, _>(
			AdditiveNTTWithPrecompute::new(8).unwrap(),
		);
	}

	#[test]
	fn test_packed_ntt_over_larger_field() {
		type Packed = PackedBinaryField4x32b;

		let mut rng = StdRng::seed_from_u64(0);

		let ntt = <AdditiveNTTWithOTFCompute<BinaryField8b>>::new(8).unwrap();
		let ntt_with_precompute = <AdditiveNTTWithPrecompute<BinaryField8b>>::new(8).unwrap();
		let mut data = repeat_with(|| Packed::random(&mut rng))
			.take(1 << 4)
			.collect::<Vec<_>>();

		let mut data_copy = data.clone();
		let mut data_copy_2 = data.clone();

		ntt.inverse_transform_simple(unpack_scalars_mut(&mut data), 2)
			.unwrap();
		AdditiveNTT::<PackedBinaryField16x8b>::inverse_transform_ext(&ntt, &mut data_copy, 2)
			.unwrap();
		AdditiveNTT::<PackedBinaryField16x8b>::inverse_transform_ext(
			&ntt_with_precompute,
			&mut data_copy_2,
			2,
		)
		.unwrap();
		assert_eq!(data, data_copy);
		assert_eq!(data, data_copy_2);

		ntt.forward_transform_simple(unpack_scalars_mut(&mut data), 3)
			.unwrap();
		AdditiveNTT::<PackedBinaryField16x8b>::forward_transform_ext(&ntt, &mut data_copy, 3)
			.unwrap();
		AdditiveNTT::<PackedBinaryField16x8b>::forward_transform_ext(
			&ntt_with_precompute,
			&mut data_copy_2,
			3,
		)
		.unwrap();
		assert_eq!(data, data_copy);
		assert_eq!(data, data_copy_2);
	}

	// TODO: Write test that compares polynomial evaluation via additive NTT with naive Lagrange
	// polynomial interpolation. A randomized test should suffice for larger NTT sizes.
}
