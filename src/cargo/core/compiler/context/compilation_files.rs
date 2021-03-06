use std::collections::HashMap;
use std::env;
use std::fmt;
use std::hash::{Hash, Hasher, SipHasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use lazycell::LazyCell;
use log::info;

use super::{BuildContext, CompileKind, Context, FileFlavor, Layout};
use crate::core::compiler::{CompileMode, CompileTarget, Unit};
use crate::core::{Target, TargetKind, Workspace};
use crate::util::{self, CargoResult};

/// The `Metadata` is a hash used to make unique file names for each unit in a
/// build. It is also use for symbol mangling.
///
/// For example:
/// - A project may depend on crate `A` and crate `B`, so the package name must be in the file name.
/// - Similarly a project may depend on two versions of `A`, so the version must be in the file name.
///
/// In general this must include all things that need to be distinguished in different parts of
/// the same build. This is absolutely required or we override things before
/// we get chance to use them.
///
/// It is also used for symbol mangling, because if you have two versions of
/// the same crate linked together, their symbols need to be differentiated.
///
/// We use a hash because it is an easy way to guarantee
/// that all the inputs can be converted to a valid path.
///
/// This also acts as the main layer of caching provided by Cargo.
/// For example, we want to cache `cargo build` and `cargo doc` separately, so that running one
/// does not invalidate the artifacts for the other. We do this by including `CompileMode` in the
/// hash, thus the artifacts go in different folders and do not override each other.
/// If we don't add something that we should have, for this reason, we get the
/// correct output but rebuild more than is needed.
///
/// Some things that need to be tracked to ensure the correct output should definitely *not*
/// go in the `Metadata`. For example, the modification time of a file, should be tracked to make a
/// rebuild when the file changes. However, it would be wasteful to include in the `Metadata`. The
/// old artifacts are never going to be needed again. We can save space by just overwriting them.
/// If we add something that we should not have, for this reason, we get the correct output but take
/// more space than needed. This makes not including something in `Metadata`
/// a form of cache invalidation.
///
/// You should also avoid anything that would interfere with reproducible
/// builds. For example, *any* absolute path should be avoided. This is one
/// reason that `RUSTFLAGS` is not in `Metadata`, because it often has
/// absolute paths (like `--remap-path-prefix` which is fundamentally used for
/// reproducible builds and has absolute paths in it). Also, in some cases the
/// mangled symbols need to be stable between different builds with different
/// settings. For example, profile-guided optimizations need to swap
/// `RUSTFLAGS` between runs, but needs to keep the same symbol names.
///
/// Note that the `Fingerprint` is in charge of tracking everything needed to determine if a
/// rebuild is needed.
#[derive(Copy, Clone, Hash, Eq, PartialEq, Ord, PartialOrd)]
pub struct Metadata(u64);

impl fmt::Display for Metadata {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:016x}", self.0)
    }
}

impl fmt::Debug for Metadata {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Metadata({:016x})", self.0)
    }
}

/// Collection of information about the files emitted by the compiler, and the
/// output directory structure.
pub struct CompilationFiles<'a, 'cfg> {
    /// The target directory layout for the host (and target if it is the same as host).
    pub(super) host: Layout,
    /// The target directory layout for the target (if different from then host).
    pub(super) target: HashMap<CompileTarget, Layout>,
    /// Additional directory to include a copy of the outputs.
    export_dir: Option<PathBuf>,
    /// The root targets requested by the user on the command line (does not
    /// include dependencies).
    roots: Vec<Unit>,
    ws: &'a Workspace<'cfg>,
    /// Metadata hash to use for each unit.
    ///
    /// `None` if the unit should not use a metadata data hash (like rustdoc,
    /// or some dylibs).
    metas: HashMap<Unit, Option<Metadata>>,
    /// For each Unit, a list all files produced.
    outputs: HashMap<Unit, LazyCell<Arc<Vec<OutputFile>>>>,
}

/// Info about a single file emitted by the compiler.
#[derive(Debug)]
pub struct OutputFile {
    /// Absolute path to the file that will be produced by the build process.
    pub path: PathBuf,
    /// If it should be linked into `target`, and what it should be called
    /// (e.g., without metadata).
    pub hardlink: Option<PathBuf>,
    /// If `--out-dir` is specified, the absolute path to the exported file.
    pub export_path: Option<PathBuf>,
    /// Type of the file (library / debug symbol / else).
    pub flavor: FileFlavor,
}

impl OutputFile {
    /// Gets the hard link if present; otherwise, returns the path.
    pub fn bin_dst(&self) -> &PathBuf {
        match self.hardlink {
            Some(ref link_dst) => link_dst,
            None => &self.path,
        }
    }
}

impl<'a, 'cfg: 'a> CompilationFiles<'a, 'cfg> {
    pub(super) fn new(
        cx: &Context<'a, 'cfg>,
        host: Layout,
        target: HashMap<CompileTarget, Layout>,
    ) -> CompilationFiles<'a, 'cfg> {
        let mut metas = HashMap::new();
        for unit in &cx.bcx.roots {
            metadata_of(unit, cx, &mut metas);
        }
        let outputs = metas
            .keys()
            .cloned()
            .map(|unit| (unit, LazyCell::new()))
            .collect();
        CompilationFiles {
            ws: &cx.bcx.ws,
            host,
            target,
            export_dir: cx.bcx.build_config.export_dir.clone(),
            roots: cx.bcx.roots.clone(),
            metas,
            outputs,
        }
    }

    /// Returns the appropriate directory layout for either a plugin or not.
    pub fn layout(&self, kind: CompileKind) -> &Layout {
        match kind {
            CompileKind::Host => &self.host,
            CompileKind::Target(target) => &self.target[&target],
        }
    }

    /// Gets the metadata for a target in a specific profile.
    /// We build to the path `"{filename}-{target_metadata}"`.
    /// We use a linking step to link/copy to a predictable filename
    /// like `target/debug/libfoo.{a,so,rlib}` and such.
    ///
    /// Returns `None` if the unit should not use a metadata data hash (like
    /// rustdoc, or some dylibs).
    pub fn metadata(&self, unit: &Unit) -> Option<Metadata> {
        self.metas[unit]
    }

    /// Gets the short hash based only on the `PackageId`.
    /// Used for the metadata when `metadata` returns `None`.
    pub fn target_short_hash(&self, unit: &Unit) -> String {
        let hashable = unit.pkg.package_id().stable_hash(self.ws.root());
        util::short_hash(&hashable)
    }

    /// Returns the appropriate output directory for the specified package and
    /// target.
    pub fn out_dir(&self, unit: &Unit) -> PathBuf {
        if unit.mode.is_doc() {
            self.layout(unit.kind).doc().to_path_buf()
        } else if unit.mode.is_doc_test() {
            panic!("doc tests do not have an out dir");
        } else if unit.target.is_custom_build() {
            self.build_script_dir(unit)
        } else if unit.target.is_example() {
            self.layout(unit.kind).examples().to_path_buf()
        } else {
            self.deps_dir(unit).to_path_buf()
        }
    }

    /// Additional export directory from `--out-dir`.
    pub fn export_dir(&self) -> Option<PathBuf> {
        self.export_dir.clone()
    }

    /// Directory name to use for a package in the form `NAME-HASH`.
    pub fn pkg_dir(&self, unit: &Unit) -> String {
        let name = unit.pkg.package_id().name();
        match self.metas[unit] {
            Some(ref meta) => format!("{}-{}", name, meta),
            None => format!("{}-{}", name, self.target_short_hash(unit)),
        }
    }

    /// Returns the root of the build output tree for the host
    pub fn host_root(&self) -> &Path {
        self.host.dest()
    }

    /// Returns the host `deps` directory path.
    pub fn host_deps(&self) -> &Path {
        self.host.deps()
    }

    /// Returns the directories where Rust crate dependencies are found for the
    /// specified unit.
    pub fn deps_dir(&self, unit: &Unit) -> &Path {
        self.layout(unit.kind).deps()
    }

    /// Directory where the fingerprint for the given unit should go.
    pub fn fingerprint_dir(&self, unit: &Unit) -> PathBuf {
        let dir = self.pkg_dir(unit);
        self.layout(unit.kind).fingerprint().join(dir)
    }

    /// Path where compiler output is cached.
    pub fn message_cache_path(&self, unit: &Unit) -> PathBuf {
        self.fingerprint_dir(unit).join("output")
    }

    /// Returns the directory where a compiled build script is stored.
    /// `/path/to/target/{debug,release}/build/PKG-HASH`
    pub fn build_script_dir(&self, unit: &Unit) -> PathBuf {
        assert!(unit.target.is_custom_build());
        assert!(!unit.mode.is_run_custom_build());
        assert!(self.metas.contains_key(unit));
        let dir = self.pkg_dir(unit);
        self.layout(CompileKind::Host).build().join(dir)
    }

    /// Returns the directory where information about running a build script
    /// is stored.
    /// `/path/to/target/{debug,release}/build/PKG-HASH`
    pub fn build_script_run_dir(&self, unit: &Unit) -> PathBuf {
        assert!(unit.target.is_custom_build());
        assert!(unit.mode.is_run_custom_build());
        let dir = self.pkg_dir(unit);
        self.layout(unit.kind).build().join(dir)
    }

    /// Returns the "OUT_DIR" directory for running a build script.
    /// `/path/to/target/{debug,release}/build/PKG-HASH/out`
    pub fn build_script_out_dir(&self, unit: &Unit) -> PathBuf {
        self.build_script_run_dir(unit).join("out")
    }

    /// Returns the file stem for a given target/profile combo (with metadata).
    pub fn file_stem(&self, unit: &Unit) -> String {
        match self.metas[unit] {
            Some(ref metadata) => format!("{}-{}", unit.target.crate_name(), metadata),
            None => self.bin_stem(unit),
        }
    }

    /// Returns the path to the executable binary for the given bin target.
    ///
    /// This should only to be used when a `Unit` is not available.
    pub fn bin_link_for_target(
        &self,
        target: &Target,
        kind: CompileKind,
        bcx: &BuildContext<'_, '_>,
    ) -> CargoResult<PathBuf> {
        assert!(target.is_bin());
        let dest = self.layout(kind).dest();
        let info = bcx.target_data.info(kind);
        let file_types = info
            .file_types(
                "bin",
                FileFlavor::Normal,
                &TargetKind::Bin,
                bcx.target_data.short_name(&kind),
            )?
            .expect("target must support `bin`");

        let file_type = file_types
            .iter()
            .find(|file_type| file_type.flavor == FileFlavor::Normal)
            .expect("target must support `bin`");

        Ok(dest.join(file_type.filename(target.name())))
    }

    /// Returns the filenames that the given unit will generate.
    pub(super) fn outputs(
        &self,
        unit: &Unit,
        bcx: &BuildContext<'a, 'cfg>,
    ) -> CargoResult<Arc<Vec<OutputFile>>> {
        self.outputs[unit]
            .try_borrow_with(|| self.calc_outputs(unit, bcx))
            .map(Arc::clone)
    }

    /// Returns the bin filename for a given target, without extension and metadata.
    fn bin_stem(&self, unit: &Unit) -> String {
        if unit.target.allows_dashes() {
            unit.target.name().to_string()
        } else {
            unit.target.crate_name()
        }
    }

    /// Returns a tuple `(hard_link_dir, filename_stem)` for the primary
    /// output file for the given unit.
    ///
    /// `hard_link_dir` is the directory where the file should be hard-linked
    /// ("uplifted") to. For example, `/path/to/project/target`.
    ///
    /// `filename_stem` is the base filename without an extension.
    ///
    /// This function returns it in two parts so the caller can add
    /// prefix/suffix to filename separately.
    ///
    /// Returns an `Option` because in some cases we don't want to link
    /// (eg a dependent lib).
    fn link_stem(&self, unit: &Unit) -> Option<(PathBuf, String)> {
        let out_dir = self.out_dir(unit);
        let bin_stem = self.bin_stem(unit); // Stem without metadata.
        let file_stem = self.file_stem(unit); // Stem with metadata.

        // We currently only lift files up from the `deps` directory. If
        // it was compiled into something like `example/` or `doc/` then
        // we don't want to link it up.
        if out_dir.ends_with("deps") {
            // Don't lift up library dependencies.
            if unit.target.is_bin() || self.roots.contains(unit) || unit.target.is_dylib() {
                Some((
                    out_dir.parent().unwrap().to_owned(),
                    if unit.mode.is_any_test() {
                        file_stem
                    } else {
                        bin_stem
                    },
                ))
            } else {
                None
            }
        } else if bin_stem == file_stem {
            None
        } else if out_dir.ends_with("examples") || out_dir.parent().unwrap().ends_with("build") {
            Some((out_dir, bin_stem))
        } else {
            None
        }
    }

    fn calc_outputs(
        &self,
        unit: &Unit,
        bcx: &BuildContext<'a, 'cfg>,
    ) -> CargoResult<Arc<Vec<OutputFile>>> {
        let ret = match unit.mode {
            CompileMode::Check { .. } => {
                // This may be confusing. rustc outputs a file named `lib*.rmeta`
                // for both libraries and binaries.
                let file_stem = self.file_stem(unit);
                let path = self.out_dir(unit).join(format!("lib{}.rmeta", file_stem));
                vec![OutputFile {
                    path,
                    hardlink: None,
                    export_path: None,
                    flavor: FileFlavor::Linkable { rmeta: false },
                }]
            }
            CompileMode::Doc { .. } => {
                let path = self
                    .out_dir(unit)
                    .join(unit.target.crate_name())
                    .join("index.html");
                vec![OutputFile {
                    path,
                    hardlink: None,
                    export_path: None,
                    flavor: FileFlavor::Normal,
                }]
            }
            CompileMode::RunCustomBuild => {
                // At this time, this code path does not handle build script
                // outputs.
                vec![]
            }
            CompileMode::Doctest => {
                // Doctests are built in a temporary directory and then
                // deleted. There is the `--persist-doctests` unstable flag,
                // but Cargo does not know about that.
                vec![]
            }
            CompileMode::Test | CompileMode::Build | CompileMode::Bench => {
                self.calc_outputs_rustc(unit, bcx)?
            }
        };
        info!("Target filenames: {:?}", ret);

        Ok(Arc::new(ret))
    }

    fn calc_outputs_rustc(
        &self,
        unit: &Unit,
        bcx: &BuildContext<'a, 'cfg>,
    ) -> CargoResult<Vec<OutputFile>> {
        let mut ret = Vec::new();
        let mut unsupported = Vec::new();

        let out_dir = self.out_dir(unit);
        let link_stem = self.link_stem(unit);
        let info = bcx.target_data.info(unit.kind);
        let file_stem = self.file_stem(unit);

        let mut add = |crate_type: &str, flavor: FileFlavor| -> CargoResult<()> {
            let crate_type = if crate_type == "lib" {
                "rlib"
            } else {
                crate_type
            };
            let file_types = info.file_types(
                crate_type,
                flavor,
                unit.target.kind(),
                bcx.target_data.short_name(&unit.kind),
            )?;

            match file_types {
                Some(types) => {
                    for file_type in types {
                        let path = out_dir.join(file_type.filename(&file_stem));
                        // Don't create hardlink for tests
                        let hardlink = if unit.mode.is_any_test() {
                            None
                        } else {
                            link_stem
                                .as_ref()
                                .map(|&(ref ld, ref ls)| ld.join(file_type.filename(ls)))
                        };
                        let export_path = if unit.target.is_custom_build() {
                            None
                        } else {
                            self.export_dir.as_ref().and_then(|export_dir| {
                                hardlink
                                    .as_ref()
                                    .map(|hardlink| export_dir.join(hardlink.file_name().unwrap()))
                            })
                        };
                        ret.push(OutputFile {
                            path,
                            hardlink,
                            export_path,
                            flavor: file_type.flavor,
                        });
                    }
                }
                // Not supported; don't worry about it.
                None => {
                    unsupported.push(crate_type.to_string());
                }
            }
            Ok(())
        };
        match *unit.target.kind() {
            TargetKind::Bin
            | TargetKind::CustomBuild
            | TargetKind::ExampleBin
            | TargetKind::Bench
            | TargetKind::Test => {
                add("bin", FileFlavor::Normal)?;
            }
            TargetKind::Lib(..) | TargetKind::ExampleLib(..) if unit.mode.is_any_test() => {
                add("bin", FileFlavor::Normal)?;
            }
            TargetKind::ExampleLib(ref kinds) | TargetKind::Lib(ref kinds) => {
                for kind in kinds {
                    add(
                        kind.crate_type(),
                        if kind.linkable() {
                            FileFlavor::Linkable { rmeta: false }
                        } else {
                            FileFlavor::Normal
                        },
                    )?;
                }
                let path = out_dir.join(format!("lib{}.rmeta", file_stem));
                if !unit.requires_upstream_objects() {
                    ret.push(OutputFile {
                        path,
                        hardlink: None,
                        export_path: None,
                        flavor: FileFlavor::Linkable { rmeta: true },
                    });
                }
            }
        }
        if ret.is_empty() {
            if !unsupported.is_empty() {
                anyhow::bail!(
                    "cannot produce {} for `{}` as the target `{}` \
                     does not support these crate types",
                    unsupported.join(", "),
                    unit.pkg,
                    bcx.target_data.short_name(&unit.kind),
                )
            }
            anyhow::bail!(
                "cannot compile `{}` as the target `{}` does not \
                 support any of the output crate types",
                unit.pkg,
                bcx.target_data.short_name(&unit.kind),
            );
        }
        Ok(ret)
    }
}

fn metadata_of(
    unit: &Unit,
    cx: &Context<'_, '_>,
    metas: &mut HashMap<Unit, Option<Metadata>>,
) -> Option<Metadata> {
    if !metas.contains_key(unit) {
        let meta = compute_metadata(unit, cx, metas);
        metas.insert(unit.clone(), meta);
        for dep in cx.unit_deps(unit) {
            metadata_of(&dep.unit, cx, metas);
        }
    }
    metas[unit]
}

fn compute_metadata(
    unit: &Unit,
    cx: &Context<'_, '_>,
    metas: &mut HashMap<Unit, Option<Metadata>>,
) -> Option<Metadata> {
    if unit.mode.is_doc_test() {
        // Doc tests do not have metadata.
        return None;
    }
    // No metadata for dylibs because of a couple issues:
    // - macOS encodes the dylib name in the executable,
    // - Windows rustc multiple files of which we can't easily link all of them.
    //
    // No metadata for bin because of an issue:
    // - wasm32 rustc/emcc encodes the `.wasm` name in the `.js` (rust-lang/cargo#4535).
    // - msvc: The path to the PDB is embedded in the executable, and we don't
    //   want the PDB path to include the hash in it.
    //
    // Two exceptions:
    // 1) Upstream dependencies (we aren't exporting + need to resolve name conflict),
    // 2) `__CARGO_DEFAULT_LIB_METADATA` env var.
    //
    // Note, however, that the compiler's build system at least wants
    // path dependencies (eg libstd) to have hashes in filenames. To account for
    // that we have an extra hack here which reads the
    // `__CARGO_DEFAULT_LIB_METADATA` environment variable and creates a
    // hash in the filename if that's present.
    //
    // This environment variable should not be relied on! It's
    // just here for rustbuild. We need a more principled method
    // doing this eventually.
    let bcx = &cx.bcx;
    let __cargo_default_lib_metadata = env::var("__CARGO_DEFAULT_LIB_METADATA");
    let short_name = bcx.target_data.short_name(&unit.kind);
    if !(unit.mode.is_any_test() || unit.mode.is_check())
        && (unit.target.is_dylib()
            || unit.target.is_cdylib()
            || (unit.target.is_executable() && short_name.starts_with("wasm32-"))
            || (unit.target.is_executable() && short_name.contains("msvc")))
        && unit.pkg.package_id().source_id().is_path()
        && __cargo_default_lib_metadata.is_err()
    {
        return None;
    }

    let mut hasher = SipHasher::new();

    // This is a generic version number that can be changed to make
    // backwards-incompatible changes to any file structures in the output
    // directory. For example, the fingerprint files or the build-script
    // output files. Normally cargo updates ship with rustc updates which will
    // cause a new hash due to the rustc version changing, but this allows
    // cargo to be extra careful to deal with different versions of cargo that
    // use the same rustc version.
    1.hash(&mut hasher);

    // Unique metadata per (name, source, version) triple. This'll allow us
    // to pull crates from anywhere without worrying about conflicts.
    unit.pkg
        .package_id()
        .stable_hash(bcx.ws.root())
        .hash(&mut hasher);

    // Also mix in enabled features to our metadata. This'll ensure that
    // when changing feature sets each lib is separately cached.
    unit.features.hash(&mut hasher);

    // Mix in the target-metadata of all the dependencies of this target.
    let mut deps_metadata = cx
        .unit_deps(unit)
        .iter()
        .map(|dep| metadata_of(&dep.unit, cx, metas))
        .collect::<Vec<_>>();
    deps_metadata.sort();
    deps_metadata.hash(&mut hasher);

    // Throw in the profile we're compiling with. This helps caching
    // `panic=abort` and `panic=unwind` artifacts, additionally with various
    // settings like debuginfo and whatnot.
    unit.profile.hash(&mut hasher);
    unit.mode.hash(&mut hasher);

    // Artifacts compiled for the host should have a different metadata
    // piece than those compiled for the target, so make sure we throw in
    // the unit's `kind` as well
    unit.kind.hash(&mut hasher);

    // Finally throw in the target name/kind. This ensures that concurrent
    // compiles of targets in the same crate don't collide.
    unit.target.name().hash(&mut hasher);
    unit.target.kind().hash(&mut hasher);

    hash_rustc_version(bcx, &mut hasher);

    if cx.bcx.ws.is_member(&unit.pkg) {
        // This is primarily here for clippy. This ensures that the clippy
        // artifacts are separate from the `check` ones.
        if let Some(path) = &cx.bcx.rustc().workspace_wrapper {
            path.hash(&mut hasher);
        }
    }

    // Seed the contents of `__CARGO_DEFAULT_LIB_METADATA` to the hasher if present.
    // This should be the release channel, to get a different hash for each channel.
    if let Ok(ref channel) = __cargo_default_lib_metadata {
        channel.hash(&mut hasher);
    }

    // std units need to be kept separate from user dependencies. std crates
    // are differentiated in the Unit with `is_std` (for things like
    // `-Zforce-unstable-if-unmarked`), so they are always built separately.
    // This isn't strictly necessary for build dependencies which probably
    // don't need unstable support. A future experiment might be to set
    // `is_std` to false for build dependencies so that they can be shared
    // with user dependencies.
    unit.is_std.hash(&mut hasher);

    Some(Metadata(hasher.finish()))
}

fn hash_rustc_version(bcx: &BuildContext<'_, '_>, hasher: &mut SipHasher) {
    let vers = &bcx.rustc().version;
    if vers.pre.is_empty() || bcx.config.cli_unstable().separate_nightlies {
        // For stable, keep the artifacts separate. This helps if someone is
        // testing multiple versions, to avoid recompiles.
        bcx.rustc().verbose_version.hash(hasher);
        return;
    }
    // On "nightly"/"beta"/"dev"/etc, keep each "channel" separate. Don't hash
    // the date/git information, so that whenever someone updates "nightly",
    // they won't have a bunch of stale artifacts in the target directory.
    //
    // This assumes that the first segment is the important bit ("nightly",
    // "beta", "dev", etc.). Skip other parts like the `.3` in `-beta.3`.
    vers.pre[0].hash(hasher);
    // Keep "host" since some people switch hosts to implicitly change
    // targets, (like gnu vs musl or gnu vs msvc). In the future, we may want
    // to consider hashing `unit.kind.short_name()` instead.
    bcx.rustc().host.hash(hasher);
    // None of the other lines are important. Currently they are:
    // binary: rustc  <-- or "rustdoc"
    // commit-hash: 38114ff16e7856f98b2b4be7ab4cd29b38bed59a
    // commit-date: 2020-03-21
    // host: x86_64-apple-darwin
    // release: 1.44.0-nightly
    // LLVM version: 9.0
    //
    // The backend version ("LLVM version") might become more relevant in
    // the future when cranelift sees more use, and people want to switch
    // between different backends without recompiling.
}
