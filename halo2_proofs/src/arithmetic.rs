//! This module provides common utilities, traits and structures for group,
//! field and polynomial arithmetic.
#[cfg(feature = "profile")]
use ark_std::{end_timer, start_timer};

use super::multicore;
pub use ff::Field;
use group::{
    ff::{BatchInvert, PrimeField},
    Curve, Group as _,
};

pub use halo2curves::{CurveAffine, CurveExt, FieldExt, Group};
use rayon::prelude::*;

/// TEMP
pub static mut MULTIEXP_TOTAL_TIME: usize = 0;

fn multiexp_serial<C: CurveAffine>(coeffs: &[C::Scalar], bases: &[C], acc: &mut C::Curve) {
    let coeffs: Vec<_> = coeffs.iter().map(|a| a.to_repr()).collect();

    let c = if bases.len() < 4 {
        1
    } else if bases.len() < 32 {
        3
    } else {
        (f64::from(bases.len() as u32)).ln().ceil() as usize
    };

    fn get_at<F: PrimeField>(segment: usize, c: usize, bytes: &F::Repr) -> usize {
        let skip_bits = segment * c;
        let skip_bytes = skip_bits / 8;

        if skip_bytes >= 32 {
            return 0;
        }

        let mut v = [0; 8];
        for (v, o) in v.iter_mut().zip(bytes.as_ref()[skip_bytes..].iter()) {
            *v = *o;
        }

        let mut tmp = u64::from_le_bytes(v);
        tmp >>= skip_bits - (skip_bytes * 8);
        tmp = tmp % (1 << c);

        tmp as usize
    }

    let segments = (256 / c) + 1;

    for current_segment in (0..segments).rev() {
        for _ in 0..c {
            *acc = acc.double();
        }

        #[derive(Clone, Copy)]
        enum Bucket<C: CurveAffine> {
            None,
            Affine(C),
            Projective(C::Curve),
        }

        impl<C: CurveAffine> Bucket<C> {
            fn add_assign(&mut self, other: &C) {
                *self = match *self {
                    Bucket::None => Bucket::Affine(*other),
                    Bucket::Affine(a) => Bucket::Projective(a + *other),
                    Bucket::Projective(mut a) => {
                        a += *other;
                        Bucket::Projective(a)
                    }
                }
            }

            fn add(self, mut other: C::Curve) -> C::Curve {
                match self {
                    Bucket::None => other,
                    Bucket::Affine(a) => {
                        other += a;
                        other
                    }
                    Bucket::Projective(a) => other + &a,
                }
            }
        }

        let mut buckets: Vec<Bucket<C>> = vec![Bucket::None; (1 << c) - 1];

        for (coeff, base) in coeffs.iter().zip(bases.iter()) {
            let coeff = get_at::<C::Scalar>(current_segment, c, coeff);
            if coeff != 0 {
                buckets[coeff - 1].add_assign(base);
            }
        }

        // Summation by parts
        // e.g. 3a + 2b + 1c = a +
        //                    (a) + b +
        //                    ((a) + b) + c
        let mut running_sum = C::Curve::identity();
        for exp in buckets.into_iter().rev() {
            running_sum = exp.add(running_sum);
            *acc = *acc + &running_sum;
        }
    }
}

/// Performs a small multi-exponentiation operation.
/// Uses the double-and-add algorithm with doublings shared across points.
pub fn small_multiexp<C: CurveAffine>(coeffs: &[C::Scalar], bases: &[C]) -> C::Curve {
    let coeffs: Vec<_> = coeffs.iter().map(|a| a.to_repr()).collect();
    let mut acc = C::Curve::identity();

    // for byte idx
    for byte_idx in (0..32).rev() {
        // for bit idx
        for bit_idx in (0..8).rev() {
            acc = acc.double();
            // for each coeff
            for coeff_idx in 0..coeffs.len() {
                let byte = coeffs[coeff_idx].as_ref()[byte_idx];
                if ((byte >> bit_idx) & 1) != 0 {
                    acc += bases[coeff_idx];
                }
            }
        }
    }

    acc
}

pub fn best_multiexp_gpu_cond<C: CurveAffine>(coeffs: &[C::Scalar], bases: &[C]) -> C::Curve {
    if coeffs.len() == 0 {
        C::Curve::identity()
    } else {
        cfg_if::cfg_if! {
            if #[cfg(feature = "cuda")] {
                gpu_multiexp(coeffs, bases)
            } else {
                best_multiexp(coeffs, bases)
            }
        }
    }
}

#[cfg(feature = "cuda")]
pub fn gpu_ifft<G: Group>(a: &mut [G], omega: G::Scalar, log_n: u32, divisor: G::Scalar) {
    use crate::plonk::{GPU_COND_VAR, GPU_LOCK, N_GPU};
    use ec_gpu_gen::fft::{FftKernel, SingleFftKernel};
    use ec_gpu_gen::rust_gpu_tools::Device;
    use halo2curves::bn256::Fr;

    let gpu_idx = acquire_gpu();

    let devices = Device::all();
    let device = devices[gpu_idx % devices.len()];
    let program = ec_gpu_gen::program!(device).unwrap();
    let mut kern = SingleFftKernel::<Fr>::create(program, None).expect("Cannot initialize kernel!");
    let a: &mut [Fr] = unsafe { std::mem::transmute(a) };
    let omega: &Fr = unsafe { std::mem::transmute(&omega) };
    let divisor: &Fr = unsafe { std::mem::transmute(&divisor) };
    kern.radix_ifft(a, omega, divisor, log_n)
        .expect("GPU FFT failed!");

    release_gpu(gpu_idx);
}

#[cfg(feature = "cuda")]
pub fn gpu_multiexp<C: CurveAffine>(coeffs: &[C::Scalar], bases: &[C]) -> C::Curve {
    gpu_multiexp_bound(coeffs, bases, C::Scalar::NUM_BITS as usize)
}

#[cfg(feature = "cuda")]
pub fn gpu_multiexp_bound<C: CurveAffine>(
    coeffs: &[C::Scalar],
    bases: &[C],
    max_bits: usize,
) -> C::Curve {
    use ec_gpu_gen::rust_gpu_tools::Device;
    use std::str::FromStr;

    if max_bits == 0 || coeffs.len() == 0 {
        C::Curve::identity()
    } else {
        //let timer = start_timer!(|| "msm gpu");
        let n_gpu = *crate::plonk::N_GPU;
        let part_len = (coeffs.len() + n_gpu - 1) / n_gpu;

        let c = coeffs
            .par_chunks(part_len)
            .zip(bases.par_chunks(part_len))
            .map(|(c, b)| gpu_multiexp_single_gpu_with_bound(c, b, max_bits))
            .collect::<Vec<_>>()
            .into_iter()
            .reduce(|acc, x| acc + x)
            .unwrap();

        //end_timer!(timer);
        c
    }
}

#[cfg(feature = "cuda")]
pub fn acquire_gpu() -> usize {
    use crate::plonk::{GPU_COND_VAR, GPU_LOCK};
    let mut free_gpus = GPU_LOCK.lock().unwrap();
    while free_gpus.len() == 0 {
        free_gpus = GPU_COND_VAR.wait(free_gpus).unwrap();
    }
    free_gpus.pop().unwrap()
}

#[cfg(feature = "cuda")]
pub fn release_gpu(gpu_idx: usize) {
    use crate::plonk::{GPU_COND_VAR, GPU_LOCK};
    {
        let mut free_gpus = GPU_LOCK.lock().unwrap();
        free_gpus.push(gpu_idx);
    }
    GPU_COND_VAR.notify_one();
}


#[cfg(feature = "cuda")]
pub fn gpu_multiexp_single_gpu_with_bound<C: CurveAffine>(
    coeffs: &[C::Scalar],
    bases: &[C],
    max_bits: usize,
) -> C::Curve {
    use crate::plonk::{GPU_COND_VAR, GPU_LOCK};
    use ec_gpu_gen::{
        fft::FftKernel, multiexp::SingleMultiexpKernel, rust_gpu_tools::Device, threadpool::Worker,
    };
    use group::Curve;
    use halo2curves::bn256::{G1Affine, Fr};
    // use pairing::bn256::Fr;

    if max_bits == 0 {
        C::Curve::identity()
    } else {
        let gpu_idx = acquire_gpu();

        let _coeffs: &[Fr] = unsafe { std::mem::transmute(&coeffs[..]) };
        let bases: &[G1Affine] = unsafe { std::mem::transmute(bases) };

        let devices = Device::all();
        let device = devices[gpu_idx % devices.len()];
        let programs = ec_gpu_gen::program!(device).unwrap();
        let kern = SingleMultiexpKernel::<G1Affine>::create(programs, device, None)
            .expect("Cannot initialize kernel!");

        let a = [kern.multiexp_bound(bases, _coeffs, max_bits).unwrap()];

        release_gpu(gpu_idx);

        let res: &[C::Curve] = unsafe { std::mem::transmute(&a[..]) };
        res[0]
    }
}


#[cfg(feature = "cuda")]
pub fn gpu_multiexp_bound_and_fft<C: CurveAffine>(
    coeffs: &mut [C::Scalar],
    bases: &[C],
    max_bits: usize,
    omega: &C::Scalar,
    divisor: &C::Scalar,
    log_n: u32,
) -> C::Curve {
    use crate::plonk::{GPU_COND_VAR, GPU_LOCK};
    use ec_gpu_gen::{
        fft::FftKernel, multiexp::SingleMultiexpKernel, rust_gpu_tools::Device, threadpool::Worker,
    };
    use group::Curve;
    use halo2curves::bn256::{Fr, G1Affine};

    let gpu_idx = acquire_gpu();
    let _coeffs: &mut [Fr] = unsafe { std::mem::transmute(coeffs) };
    let bases: &[G1Affine] = unsafe { std::mem::transmute(bases) };
    let omega: &Fr = unsafe { std::mem::transmute(omega) };
    let divisor: &Fr = unsafe { std::mem::transmute(divisor) };

    let devices = Device::all();
    let device = devices[gpu_idx % devices.len()];
    let programs = ec_gpu_gen::program!(device).unwrap();
    let kern = SingleMultiexpKernel::<G1Affine>::create(programs, device, None)
        .expect("Cannot initialize kernel!");

    let a = [kern
        .multiexp_bound_and_ifft(bases, _coeffs, max_bits, omega, divisor, log_n)
        .unwrap()];
    release_gpu(gpu_idx);

    let res: &[C::Curve] = unsafe { std::mem::transmute(&a[..]) };

    res[0]
}

/// Performs a multi-exponentiation operation.
///
/// This function will panic if coeffs and bases have a different length.
///
/// This will use multithreading if beneficial.
pub fn best_multiexp<C: CurveAffine>(coeffs: &[C::Scalar], bases: &[C]) -> C::Curve {
    // let profile_info= format!("multEXP====== coeffs_len {} , bases_len {}", coeffs.len(), bases.len());
    // #[cfg(feature = "profile")]
    // let best_multiexp_timer = start_timer!(|| profile_info);
   
    assert_eq!(coeffs.len(), bases.len());

    //println!("msm: {}", coeffs.len());

    let start = get_time();
    let num_threads = multicore::current_num_threads();
    let res = if coeffs.len() > num_threads {
        let chunk = coeffs.len() / num_threads;
        let num_chunks = coeffs.chunks(chunk).len();
        let mut results = vec![C::Curve::identity(); num_chunks];
        multicore::scope(|scope| {
            let chunk = coeffs.len() / num_threads;

            for ((coeffs, bases), acc) in coeffs
                .chunks(chunk)
                .zip(bases.chunks(chunk))
                .zip(results.iter_mut())
            {
                scope.spawn(move |_| {
                    multiexp_serial(coeffs, bases, acc);
                });
            }
        });
        results.iter().fold(C::Curve::identity(), |a, b| a + b)
    } else {
        let mut acc = C::Curve::identity();
        multiexp_serial(coeffs, bases, &mut acc);
        acc
    };

    let duration = get_duration(start);
    #[allow(unsafe_code)]
    unsafe {
        MULTIEXP_TOTAL_TIME += duration;
    }
    // #[cfg(feature = "profile")]
    // end_timer!(best_multiexp_timer);

    res
}

#[cfg(feature = "cuda")]
pub fn gpu_fft<G: Group>(a: &mut [G], omega: G::Scalar, log_n: u32) {
    use crate::plonk::{GPU_COND_VAR, GPU_LOCK, N_GPU};
    use ec_gpu_gen::fft::{FftKernel, SingleFftKernel};
    use ec_gpu_gen::rust_gpu_tools::Device;
    use halo2curves::bn256::Fr;

    let gpu_idx = acquire_gpu();

    let devices = Device::all();
    let device = devices[gpu_idx % devices.len()];
    let program = ec_gpu_gen::program!(device).unwrap();
    let mut kern = SingleFftKernel::<Fr>::create(program, None).expect("Cannot initialize kernel!");
    let a: &mut [Fr] = unsafe { std::mem::transmute(a) };
    let omega: &Fr = unsafe { std::mem::transmute(&omega) };
    kern.radix_fft(a, omega, log_n).expect("GPU FFT failed!");

    release_gpu(gpu_idx);
}

pub fn best_fft<G: Group>(a: &mut [G], omega: G::Scalar, log_n: u32) {
    cfg_if::cfg_if! {
        if #[cfg(feature = "cuda")]{
            return gpu_fft(a, omega, log_n);
        } else {
            return best_fft_cpu(a, omega, log_n);
        }
    }
}

/// Performs a radix-$2$ Fast-Fourier Transformation (FFT) on a vector of size
/// $n = 2^k$, when provided `log_n` = $k$ and an element of multiplicative
/// order $n$ called `omega` ($\omega$). The result is that the vector `a`, when
/// interpreted as the coefficients of a polynomial of degree $n - 1$, is
/// transformed into the evaluations of this polynomial at each of the $n$
/// distinct powers of $\omega$. This transformation is invertible by providing
/// $\omega^{-1}$ in place of $\omega$ and dividing each resulting field element
/// by $n$.
///
/// This will use multithreading if beneficial.
pub fn best_fft_cpu<G: Group>(a: &mut [G], omega: G::Scalar, log_n: u32) {
    fn bitreverse(mut n: usize, l: usize) -> usize {
        let mut r = 0;
        for _ in 0..l {
            r = (r << 1) | (n & 1);
            n >>= 1;
        }
        r
    }

    let threads = multicore::current_num_threads();
    let log_threads = log2_floor(threads);
    let n = a.len();
    assert_eq!(n, 1 << log_n);

    for k in 0..n {
        let rk = bitreverse(k, log_n as usize);
        if k < rk {
            a.swap(rk, k);
        }
    }

    //let start = start_measure(format!("twiddles {} ({})", a.len(), threads), false);
    // precompute twiddle factors
    let twiddles: Vec<_> = (0..(n / 2))
        .scan(G::Scalar::one(), |w, _| {
            let tw = *w;
            w.group_scale(&omega);
            Some(tw)
        })
        .collect();
    //stop_measure(start);

    if log_n <= log_threads {
        let mut chunk = 2_usize;
        let mut twiddle_chunk = n / 2;
        for _ in 0..log_n {
            a.chunks_mut(chunk).for_each(|coeffs| {
                let (left, right) = coeffs.split_at_mut(chunk / 2);

                // case when twiddle factor is one
                let (a, left) = left.split_at_mut(1);
                let (b, right) = right.split_at_mut(1);
                let t = b[0];
                b[0] = a[0];
                a[0].group_add(&t);
                b[0].group_sub(&t);

                left.iter_mut()
                    .zip(right.iter_mut())
                    .enumerate()
                    .for_each(|(i, (a, b))| {
                        let mut t = *b;
                        t.group_scale(&twiddles[(i + 1) * twiddle_chunk]);
                        *b = *a;
                        a.group_add(&t);
                        b.group_sub(&t);
                    });
            });
            chunk *= 2;
            twiddle_chunk /= 2;
        }
    } else {
        recursive_butterfly_arithmetic(a, n, 1, &twiddles)
    }
}

/// This perform recursive butterfly arithmetic
pub fn recursive_butterfly_arithmetic<G: Group>(
    a: &mut [G],
    n: usize,
    twiddle_chunk: usize,
    twiddles: &[G::Scalar],
) {
    if n == 2 {
        let t = a[1];
        a[1] = a[0];
        a[0].group_add(&t);
        a[1].group_sub(&t);
    } else {
        let (left, right) = a.split_at_mut(n / 2);
        rayon::join(
            || recursive_butterfly_arithmetic(left, n / 2, twiddle_chunk * 2, twiddles),
            || recursive_butterfly_arithmetic(right, n / 2, twiddle_chunk * 2, twiddles),
        );

        // case when twiddle factor is one
        let (a, left) = left.split_at_mut(1);
        let (b, right) = right.split_at_mut(1);
        let t = b[0];
        b[0] = a[0];
        a[0].group_add(&t);
        b[0].group_sub(&t);

        left.iter_mut()
            .zip(right.iter_mut())
            .enumerate()
            .for_each(|(i, (a, b))| {
                let mut t = *b;
                t.group_scale(&twiddles[(i + 1) * twiddle_chunk]);
                *b = *a;
                a.group_add(&t);
                b.group_sub(&t);
            });
    }
}

/// Convert coefficient bases group elements to lagrange basis by inverse FFT.
pub fn g_to_lagrange<C: CurveAffine>(g_projective: Vec<C::Curve>, k: u32) -> Vec<C> {
    let n_inv = C::Scalar::TWO_INV.pow_vartime([k as u64, 0, 0, 0]);
    let mut omega_inv = C::Scalar::ROOT_OF_UNITY_INV;
    for _ in k..C::Scalar::S {
        omega_inv = omega_inv.square();
    }

    let mut g_lagrange_projective = g_projective;
    best_fft(&mut g_lagrange_projective, omega_inv, k);
    parallelize(&mut g_lagrange_projective, |g, _| {
        for g in g.iter_mut() {
            *g *= n_inv;
        }
    });

    let mut g_lagrange = vec![C::identity(); 1 << k];
    parallelize(&mut g_lagrange, |g_lagrange, starts| {
        C::Curve::batch_normalize(
            &g_lagrange_projective[starts..(starts + g_lagrange.len())],
            g_lagrange,
        );
    });

    g_lagrange
}

/// This evaluates a provided polynomial (in coefficient form) at `point`.
pub fn eval_polynomial<F: Field>(poly: &[F], point: F) -> F {
    fn evaluate<F: Field>(poly: &[F], point: F) -> F {
        poly.iter()
            .rev()
            .fold(F::zero(), |acc, coeff| acc * point + coeff)
    }
    let n = poly.len();
    let num_threads = multicore::current_num_threads();
    if n * 2 < num_threads {
        evaluate(poly, point)
    } else {
        let chunk_size = (n + num_threads - 1) / num_threads;
        let mut parts = vec![F::zero(); num_threads];
        multicore::scope(|scope| {
            for (chunk_idx, (out, poly)) in
                parts.chunks_mut(1).zip(poly.chunks(chunk_size)).enumerate()
            {
                scope.spawn(move |_| {
                    let start = chunk_idx * chunk_size;
                    out[0] = evaluate(poly, point) * point.pow_vartime([start as u64, 0, 0, 0]);
                });
            }
        });
        parts.iter().fold(F::zero(), |acc, coeff| acc + coeff)
    }
}

/// This computes the inner product of two vectors `a` and `b`.
///
/// This function will panic if the two vectors are not the same size.
pub fn compute_inner_product<F: Field>(a: &[F], b: &[F]) -> F {
    // TODO: parallelize?
    assert_eq!(a.len(), b.len());

    let mut acc = F::zero();
    for (a, b) in a.iter().zip(b.iter()) {
        acc += (*a) * (*b);
    }

    acc
}

/// Divides polynomial `a` in `X` by `X - b` with
/// no remainder.
pub fn kate_division<'a, F: Field, I: IntoIterator<Item = &'a F>>(a: I, mut b: F) -> Vec<F>
where
    I::IntoIter: DoubleEndedIterator + ExactSizeIterator,
{
    b = -b;
    let a = a.into_iter();

    let mut q = vec![F::zero(); a.len() - 1];

    let mut tmp = F::zero();
    for (q, r) in q.iter_mut().rev().zip(a.rev()) {
        let mut lead_coeff = *r;
        lead_coeff.sub_assign(&tmp);
        *q = lead_coeff;
        tmp = lead_coeff;
        tmp.mul_assign(&b);
    }

    q
}

/// This simple utility function will parallelize an operation that is to be
/// performed over a mutable slice.
pub fn parallelize<T: Send, F: Fn(&mut [T], usize) + Send + Sync + Clone>(v: &mut [T], f: F) {
    let n = v.len();
    let num_threads = multicore::current_num_threads();
    let mut chunk = n / num_threads;
    if chunk < num_threads {
        chunk = 1;
    }

    multicore::scope(|scope| {
        for (chunk_num, v) in v.chunks_mut(chunk).enumerate() {
            let f = f.clone();
            scope.spawn(move |_| {
                let start = chunk_num * chunk;
                f(v, start);
            });
        }
    });
}

/// This simple utility function will parallelize an operation that is to be
/// performed over a mutable slice.
pub fn parallelize_count<T: Send, F: Fn(&mut [T], usize) + Send + Sync + Clone>(
    v: &mut [T],
    num_threads: usize,
    f: F,
) {
    let n = v.len();
    let mut chunk = n / num_threads;
    if chunk < num_threads {
        chunk = n;
    }

    multicore::scope(|scope| {
        for (chunk_num, v) in v.chunks_mut(chunk).enumerate() {
            let f = f.clone();
            scope.spawn(move |_| {
                f(v, chunk_num);
            });
        }
    });
}

fn log2_floor(num: usize) -> u32 {
    assert!(num > 0);

    let mut pow = 0;

    while (1 << (pow + 1)) <= num {
        pow += 1;
    }

    pow
}

/// Returns coefficients of an n - 1 degree polynomial given a set of n points
/// and their evaluations. This function will panic if two values in `points`
/// are the same.
pub fn lagrange_interpolate<F: FieldExt>(points: &[F], evals: &[F]) -> Vec<F> {
    assert_eq!(points.len(), evals.len());
    if points.len() == 1 {
        // Constant polynomial
        vec![evals[0]]
    } else {
        let mut denoms = Vec::with_capacity(points.len());
        for (j, x_j) in points.iter().enumerate() {
            let mut denom = Vec::with_capacity(points.len() - 1);
            for x_k in points
                .iter()
                .enumerate()
                .filter(|&(k, _)| k != j)
                .map(|a| a.1)
            {
                denom.push(*x_j - x_k);
            }
            denoms.push(denom);
        }
        // Compute (x_j - x_k)^(-1) for each j != i
        denoms.iter_mut().flat_map(|v| v.iter_mut()).batch_invert();

        let mut final_poly = vec![F::zero(); points.len()];
        for (j, (denoms, eval)) in denoms.into_iter().zip(evals.iter()).enumerate() {
            let mut tmp: Vec<F> = Vec::with_capacity(points.len());
            let mut product = Vec::with_capacity(points.len() - 1);
            tmp.push(F::one());
            for (x_k, denom) in points
                .iter()
                .enumerate()
                .filter(|&(k, _)| k != j)
                .map(|a| a.1)
                .zip(denoms.into_iter())
            {
                product.resize(tmp.len() + 1, F::zero());
                for ((a, b), product) in tmp
                    .iter()
                    .chain(std::iter::once(&F::zero()))
                    .zip(std::iter::once(&F::zero()).chain(tmp.iter()))
                    .zip(product.iter_mut())
                {
                    *product = *a * (-denom * x_k) + *b * denom;
                }
                std::mem::swap(&mut tmp, &mut product);
            }
            assert_eq!(tmp.len(), points.len());
            assert_eq!(product.len(), points.len() - 1);
            for (final_coeff, interpolation_coeff) in final_poly.iter_mut().zip(tmp.into_iter()) {
                *final_coeff += interpolation_coeff * eval;
            }
        }
        final_poly
    }
}

pub(crate) fn evaluate_vanishing_polynomial<F: FieldExt>(roots: &[F], z: F) -> F {
    fn evaluate<F: FieldExt>(roots: &[F], z: F) -> F {
        roots.iter().fold(F::one(), |acc, point| (z - point) * acc)
    }
    let n = roots.len();
    let num_threads = multicore::current_num_threads();
    if n * 2 < num_threads {
        evaluate(roots, z)
    } else {
        let chunk_size = (n + num_threads - 1) / num_threads;
        let mut parts = vec![F::one(); num_threads];
        multicore::scope(|scope| {
            for (out, roots) in parts.chunks_mut(1).zip(roots.chunks(chunk_size)) {
                scope.spawn(move |_| out[0] = evaluate(roots, z));
            }
        });
        parts.iter().fold(F::one(), |acc, part| acc * part)
    }
}

pub(crate) fn powers<F: FieldExt>(base: F) -> impl Iterator<Item = F> {
    std::iter::successors(Some(F::one()), move |power| Some(base * power))
}

#[cfg(test)]
use rand_core::OsRng;

#[cfg(test)]
use crate::halo2curves::pasta::Fp;
use crate::plonk::{get_duration, get_time, start_measure, stop_measure};

#[test]
fn test_lagrange_interpolate() {
    let rng = OsRng;

    let points = (0..5).map(|_| Fp::random(rng)).collect::<Vec<_>>();
    let evals = (0..5).map(|_| Fp::random(rng)).collect::<Vec<_>>();

    for coeffs in 0..5 {
        let points = &points[0..coeffs];
        let evals = &evals[0..coeffs];

        let poly = lagrange_interpolate(points, evals);
        assert_eq!(poly.len(), points.len());

        for (point, eval) in points.iter().zip(evals) {
            assert_eq!(eval_polynomial(&poly, *point), *eval);
        }
    }
}
