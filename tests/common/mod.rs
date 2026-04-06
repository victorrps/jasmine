/// Minimal valid PDF bytes for testing.
#[allow(dead_code)]
pub fn sample_pdf_bytes() -> Vec<u8> {
    include_bytes!("../fixtures/sample.pdf").to_vec()
}
