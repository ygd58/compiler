use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
    rc::Rc,
    sync::Arc,
};

use anyhow::{Context as _, Result, anyhow};
use clap::Args;
use miden_mast_package::{Package, Section, SectionId};
use midenc_compile::{CompiledArtifact, Compiler};
use midenc_frontend_wasm_metadata::PACKAGE_NOTE_CODEC_SECTION_ID;
use midenc_session::{InputFile, diagnostics::PrintDiagnostic};
use wit_component::{ComponentEncoder, DecodedWasm};
use wit_parser::WorldItem;

/// Metadata table that points to an author-side note codec crate.
const NOTE_CODEC_CRATE_METADATA: &str = "note-codec-crate";

/// Metadata field that contains the codec crate directory.
const NOTE_CODEC_CRATE_PATH: &str = "path";

/// Rust target used for zero-import note codec components.
const NOTE_CODEC_TARGET: &str = "wasm32-unknown-unknown";

/// Command-line arguments accepted by `cargo miden build`.
///
/// All arguments following `build` are parsed by the `midenc` compiler's argument parser.
/// Cargo-specific options (`--release`, `--manifest-path`, `--workspace`, `--package`)
/// are recognized and forwarded to the underlying `cargo build` invocation.
/// All other options are passed to `midenc` for compilation.
#[derive(Clone, Debug, Args)]
#[command(disable_version_flag = true, trailing_var_arg = true)]
pub struct BuildCommand {
    /// Arguments parsed by midenc (includes cargo-compatible options).
    #[arg(value_name = "ARG", allow_hyphen_values = true)]
    pub args: Vec<String>,
}

impl BuildCommand {
    /// Executes `cargo miden build`, returning the resulting command output.
    pub fn exec(self) -> Result<PathBuf> {
        // Parse all arguments using midenc's Compiler parser.
        // This gives us a structured representation of all options.
        let cwd = std::env::current_dir()?;
        let compiler_opts =
            Compiler::try_parse_from(cwd.clone(), &self.args).unwrap_or_else(|err| err.exit());

        let metadata_out_dir = compiler_opts.target_dir.join(&compiler_opts.profile);

        let manifest_path = match compiler_opts.manifest_path.as_deref() {
            Some(manifest_path) => manifest_path.to_path_buf(),
            None => cwd.join("Cargo.toml"),
        };
        let manifest_path = if manifest_path.is_absolute() {
            manifest_path
        } else {
            cwd.join(manifest_path)
        };
        let project_dir = manifest_path.parent().ok_or_else(|| {
            anyhow!("Cargo manifest '{}' has no parent directory", manifest_path.display())
        })?;
        let project_manifest_path = project_dir.join("miden-project.toml");
        let input = InputFile::from_path(&manifest_path).unwrap();
        let session = Rc::new(
            compiler_opts
                .into_session(input, None, None)
                .map_err(|err| anyhow!("{}", PrintDiagnostic::new(err)))?,
        );
        let source_manager = Arc::clone(&session.source_manager);

        let artifact =
            midenc_compile::compile_to_memory(Rc::new(midenc_hir::Context::new(session)))
                .map_err(|err| anyhow!("{}", PrintDiagnostic::new(err)))?;

        match artifact {
            CompiledArtifact::Assembled(package) => {
                let mut package = Arc::unwrap_or_clone(package);
                if let Some(codec_crate_dir) =
                    note_codec_crate_dir(&project_manifest_path, source_manager.as_ref())?
                {
                    let component =
                        build_note_codec_component(&codec_crate_dir, project_dir, &package)?;
                    let section_id = SectionId::custom(PACKAGE_NOTE_CODEC_SECTION_ID)
                        .context("the note codec package section id is invalid")?;
                    package.sections.push(Section::new(section_id, component));
                }

                // Written atomically: dependent projects deserialize this artifact from disk
                // while expanding their own macros, potentially in parallel with a rebuild.
                let output_path =
                    midenc_compile::cargo::write_package_atomic(&package, &metadata_out_dir)
                        .map_err(|err| anyhow!("{}", PrintDiagnostic::new(err)))
                        .with_context(|| {
                            format!(
                                "failed to write package artifact for {}@{}",
                                &package.name, &package.version
                            )
                        })?;
                Ok(output_path)
            }
            _ => unreachable!(),
        }
    }
}

/// Reads the optional codec crate path from a Miden project.
fn note_codec_crate_dir(
    project_manifest_path: &Path,
    source_manager: &dyn midenc_session::SourceManager,
) -> Result<Option<PathBuf>> {
    let project =
        miden_project::Project::load(project_manifest_path, source_manager).map_err(|error| {
            anyhow!(
                "failed to read note codec metadata from '{}': {}",
                project_manifest_path.display(),
                PrintDiagnostic::new(error)
            )
        })?;
    let package = project.package();
    let metadata: &miden_project::MetadataSet = package.metadata();
    let Some(codec_metadata) = metadata.get(NOTE_CODEC_CRATE_METADATA) else {
        return Ok(None);
    };
    let path = codec_metadata
        .get(NOTE_CODEC_CRATE_PATH)
        .ok_or_else(|| {
            anyhow!(
                "`[package.metadata.{NOTE_CODEC_CRATE_METADATA}]` in '{}' must define a string \
                 `{NOTE_CODEC_CRATE_PATH}`",
                project_manifest_path.display()
            )
        })?
        .inner()
        .as_str()
        .ok_or_else(|| {
            anyhow!(
                "`[package.metadata.{NOTE_CODEC_CRATE_METADATA}].{NOTE_CODEC_CRATE_PATH}` in '{}' \
                 must be a string",
                project_manifest_path.display()
            )
        })?;
    if path.is_empty() {
        return Err(anyhow!(
            "`[package.metadata.{NOTE_CODEC_CRATE_METADATA}].{NOTE_CODEC_CRATE_PATH}` in '{}' \
             must not be empty",
            project_manifest_path.display()
        ));
    }

    let project_dir = project_manifest_path
        .parent()
        .expect("a loaded project manifest always has a parent");
    let codec_crate_dir = project_dir.join(path);
    let codec_crate_dir = codec_crate_dir.canonicalize().with_context(|| {
        format!(
            "note codec crate '{}' does not exist; update \
             `[package.metadata.{NOTE_CODEC_CRATE_METADATA}].{NOTE_CODEC_CRATE_PATH}` in '{}'",
            codec_crate_dir.display(),
            project_manifest_path.display()
        )
    })?;
    if !codec_crate_dir.is_dir() {
        return Err(anyhow!(
            "note codec crate path '{}' is not a directory",
            codec_crate_dir.display()
        ));
    }
    Ok(Some(codec_crate_dir))
}

/// Builds and componentizes one author-side note codec crate.
fn build_note_codec_component(
    codec_crate_dir: &Path,
    note_project_dir: &Path,
    note_package: &Package,
) -> Result<Vec<u8>> {
    let _staged_package = stage_note_package(note_project_dir, note_package)?;
    let manifest_path = codec_crate_dir.join("Cargo.toml");
    let artifact_name = codec_artifact_name(&manifest_path)?;
    let target_dir = codec_crate_dir.join("target");
    let wasm_path = target_dir
        .join(NOTE_CODEC_TARGET)
        .join("release")
        .join(artifact_name)
        .with_extension("wasm");
    // Cargo does not track a package file that a procedural macro reads. Remove the final output
    // so the codec crate expands against the package staged above.
    match fs::remove_file(&wasm_path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| {
                format!("failed to prepare note codec output '{}'", wasm_path.display())
            });
        }
    }
    let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let output = Command::new(cargo)
        .current_dir(codec_crate_dir)
        .args([
            "build",
            "--manifest-path",
            manifest_path.to_str().ok_or_else(|| {
                anyhow!("codec manifest path '{}' is not UTF-8", manifest_path.display())
            })?,
            "--lib",
            "--release",
            "--target",
            NOTE_CODEC_TARGET,
            "--target-dir",
            target_dir.to_str().ok_or_else(|| {
                anyhow!("codec target path '{}' is not UTF-8", target_dir.display())
            })?,
        ])
        .env_remove("CARGO_BUILD_TARGET")
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env_remove("RUSTFLAGS")
        .output()
        .with_context(|| {
            format!(
                "failed to start `cargo build` for note codec crate '{}'",
                codec_crate_dir.display()
            )
        })?;
    if !output.status.success() {
        return Err(anyhow!(
            "failed to build note codec crate '{}' for {NOTE_CODEC_TARGET} in release mode \
             (install the target with `rustup target add {NOTE_CODEC_TARGET}` if \
             needed)\nstdout:\n{}\nstderr:\n{}",
            codec_crate_dir.display(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        ));
    }

    let module = fs::read(&wasm_path).with_context(|| {
        format!(
            "note codec build succeeded but did not produce the expected cdylib '{}'",
            wasm_path.display()
        )
    })?;
    let component = ComponentEncoder::default()
        .module(&module)
        .with_context(|| {
            format!("note codec module '{}' is not component-ready", wasm_path.display())
        })?
        .validate(true)
        .encode()
        .with_context(|| {
            format!("failed to encode note codec module '{}' as a component", wasm_path.display())
        })?;
    validate_note_codec_component(&component).with_context(|| {
        format!("note codec crate '{}' produced an invalid component", codec_crate_dir.display())
    })?;
    Ok(component)
}

/// Stages the current in-memory note package for `from_project!` during the codec build.
fn stage_note_package(
    note_project_dir: &Path,
    note_package: &Package,
) -> Result<tempfile::TempDir> {
    let profiles_dir = note_project_dir.join("target/miden");
    fs::create_dir_all(&profiles_dir).with_context(|| {
        format!("failed to create note package staging root '{}'", profiles_dir.display())
    })?;
    let staging_dir = tempfile::Builder::new()
        .prefix("zz-note-codec-input-")
        .tempdir_in(&profiles_dir)
        .with_context(|| {
            format!(
                "failed to create note package staging directory in '{}'",
                profiles_dir.display()
            )
        })?;
    note_package.write_masp_file(staging_dir.path()).with_context(|| {
        format!(
            "failed to stage note package {}@{} for codec generation",
            note_package.name, note_package.version
        )
    })?;
    Ok(staging_dir)
}

/// Returns the expected Wasm artifact name and checks the cdylib configuration.
fn codec_artifact_name(manifest_path: &Path) -> Result<String> {
    let source = fs::read_to_string(manifest_path).with_context(|| {
        format!("note codec crate has no readable manifest at '{}'", manifest_path.display())
    })?;
    let manifest = source.parse::<toml_edit::DocumentMut>().with_context(|| {
        format!("failed to parse note codec manifest '{}'", manifest_path.display())
    })?;
    let package =
        manifest
            .get("package")
            .and_then(toml_edit::Item::as_table_like)
            .ok_or_else(|| {
                anyhow!("codec manifest '{}' has no `[package]` table", manifest_path.display())
            })?;
    let package_name = package.get("name").and_then(toml_edit::Item::as_str).ok_or_else(|| {
        anyhow!("codec manifest '{}' has no package name", manifest_path.display())
    })?;
    let lib = manifest.get("lib").and_then(toml_edit::Item::as_table_like).ok_or_else(|| {
        anyhow!("codec manifest '{}' has no `[lib]` table", manifest_path.display())
    })?;
    let is_cdylib =
        lib.get("crate-type")
            .and_then(toml_edit::Item::as_array)
            .is_some_and(|crate_types| {
                crate_types.iter().any(|crate_type| crate_type.as_str() == Some("cdylib"))
            });
    if !is_cdylib {
        return Err(anyhow!(
            "note codec manifest '{}' must set `[lib] crate-type = [\"cdylib\"]`",
            manifest_path.display()
        ));
    }
    let lib_name = lib.get("name").and_then(toml_edit::Item::as_str).unwrap_or(package_name);
    Ok(lib_name.replace('-', "_"))
}

/// Verifies the component sandbox and the versioned codec interface export.
fn validate_note_codec_component(component: &[u8]) -> Result<()> {
    let DecodedWasm::Component(resolve, world_id) = wit_component::decode(component)
        .context("failed to decode the encoded note codec component")?
    else {
        return Err(anyhow!("note codec output is not a component"));
    };
    let world = &resolve.worlds[world_id];
    if !world.imports.is_empty() {
        return Err(anyhow!(
            "note codec component must have zero imports, found: {:#?}",
            world.imports
        ));
    }
    if world.exports.len() != 1 {
        return Err(anyhow!(
            "note codec component must export only `miden:note-codec/codec@1.0.0`, found: {:#?}",
            world.exports
        ));
    }

    let interface = world.exports.values().find_map(|item| {
        let WorldItem::Interface { id, .. } = item else {
            return None;
        };
        let interface = &resolve.interfaces[*id];
        let package_id = interface.package?;
        let package = &resolve.packages[package_id].name;
        (interface.name.as_deref() == Some("codec")
            && package.namespace == "miden"
            && package.name == "note-codec"
            && package.version.as_ref().is_some_and(|version| version.to_string() == "1.0.0"))
        .then_some(interface)
    });
    let interface = interface
        .ok_or_else(|| anyhow!("component does not export `miden:note-codec/codec@1.0.0`"))?;
    for function in ["supported-types", "parse", "display", "validate"] {
        if !interface.functions.contains_key(function) {
            return Err(anyhow!("`miden:note-codec/codec@1.0.0` is missing `{function}`"));
        }
    }
    Ok(())
}
