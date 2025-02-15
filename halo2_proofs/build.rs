fn main() {
    #[cfg(feature = "cuda")]
    {
        use ec_gpu_gen::SourceBuilder;
        use halo2curves::bn256::{G1Affine, Fr, Fq};
        // use pairing::bn256::{Fq, Fr, G1Affine};
        let source_builder = SourceBuilder::new()
            .add_fft::<Fr>()
            .add_multiexp::<G1Affine, Fq>();
        ec_gpu_gen::generate(&source_builder);
    }
}
