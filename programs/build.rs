//! Embeds the committed guest ELFs and derives their image ids.
//!
//! Vendored rather than reusing upstream's `build_utils::include_artifacts`,
//! because that helper resolves `../artifacts/` against *its own*
//! `CARGO_MANIFEST_DIR` (baked in at compile time), which points into the LEZ
//! checkout and could never find lpad's artifacts.
//!
//! Only runs under the `artifacts` feature: during a guest cross-compile
//! (`--features programs`) the `.bin` files are the build *output*, so reading
//! them would be circular.

fn main() {
    #[cfg(feature = "artifacts")]
    if let Err(e) = artifacts::generate() {
        // A missing artifacts dir is the normal state of a fresh clone, so fail
        // with the fix rather than a bare io error.
        panic!(
            "failed to embed guest artifacts: {e}\n\
             Build them first:  bash scripts/build-guests.sh"
        );
    }
}

#[cfg(feature = "artifacts")]
mod artifacts {
    use std::{env, fmt::Write as _, fs, path::PathBuf};

    pub fn generate() -> Result<(), Box<dyn std::error::Error>> {
        // CARGO_MANIFEST_DIR is `programs/`, so `artifacts/lpad` is a sibling of
        // the program crates.
        let artifacts_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("artifacts/lpad");
        let out_dir = PathBuf::from(env::var("OUT_DIR")?);
        let mod_dir = out_dir.join("lpad");

        println!("cargo:rerun-if-changed={}", artifacts_dir.display());

        let mut bins: Vec<PathBuf> = fs::read_dir(&artifacts_dir)
            .map_err(|e| format!("read {}: {e}", artifacts_dir.display()))?
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "bin"))
            .collect();
        // Deterministic order so the generated module is reproducible.
        bins.sort();

        if bins.is_empty() {
            return Err(format!("no .bin files in {}", artifacts_dir.display()).into());
        }

        fs::create_dir_all(&mod_dir)?;
        let mut src = String::new();
        for path in bins {
            let name = path
                .file_stem()
                .ok_or("artifact has no file stem")?
                .to_string_lossy()
                .to_uppercase();
            let bytes = fs::read(&path)?;
            let image_id: [u32; 8] = risc0_binfmt::compute_image_id(&bytes)
                .map_err(|e| format!("image id for {}: {e}", path.display()))?
                .into();
            writeln!(
                src,
                "pub const {name}_ELF: &[u8] = include_bytes!(r#\"{}\"#);\n\
                 #[expect(clippy::unreadable_literal, reason = \"risc0 image id\")]\n\
                 pub const {name}_ID: [u32; 8] = {image_id:?};",
                path.display()
            )?;
        }
        fs::write(mod_dir.join("mod.rs"), src)?;
        Ok(())
    }
}
