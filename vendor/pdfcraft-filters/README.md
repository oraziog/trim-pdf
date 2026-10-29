# pdfcraft-filters (vendored)

Source: <https://github.com/storytold/pdfcraft> at commit `1dd2737b47a9c1c9562f0af5630531bf510e4bf`,
directory `crates/filters`. Licence: MIT OR Apache-2.0 (see `LICENSE-MIT`, `LICENSE-APACHE` and the
repository's `NOTICE`), same terms as this project.

Why it is here: the splitter needs PDF stream filters — `FlateDecode` with TIFF/PNG predictors, the
filters an object stream and a cross-reference stream use — and that crate is standalone (layer L0,
no other PdfCraft crate) with a small, auditable dependency set (`flate2` with the Rust backend,
`thiserror`). Upstream's own workspace does not build as a whole at this commit, so this copy is
kept beside our crates instead of being pulled through a path dependency on a full checkout.

Files are copied verbatim; only `Cargo.toml` differs (the workspace-inherited fields are written
out). Updating means re-copying `src/` from a pinned upstream commit and re-checking this note.
