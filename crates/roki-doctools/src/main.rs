use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand};
use serde::Deserialize;
use walkdir::WalkDir;

const DEFAULT_DOC_ROOT: &str = "docs";

const SKIP_DIRS: &[&str] = &[".git", "target", "node_modules"];

#[derive(Parser)]
#[command(
    name = "roki-doctools",
    about = "Cross-reference graph tooling for roki specs and docs"
)]
struct Cli {
    #[arg(long, value_name = "DIR", global = true)]
    root: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Validate the doc graph (dangling refs, dup IDs, unknown kinds, missing modules).
    Validate,
    /// Print everything that depends on the given IDs (forward impact, transitive).
    Impact {
        ids: Vec<String>,
        #[arg(long, default_value_t = u32::MAX)]
        depth: u32,
        /// Include `related:` edges (soft links). Default: hard edges only.
        #[arg(long)]
        include_related: bool,
    },
    /// Print everything the given IDs depend on (reverse, transitive).
    Deps {
        ids: Vec<String>,
        #[arg(long, default_value_t = u32::MAX)]
        depth: u32,
        #[arg(long)]
        include_related: bool,
    },
    /// Print one doc: front matter + immediate forward and reverse refs.
    Show { id: String },
    /// Given source files, print docs whose `modules:` cover them, plus impact closure.
    Touched {
        files: Vec<PathBuf>,
        #[arg(long)]
        no_closure: bool,
    },
    /// Regenerate index files. Two targets:
    ///   `map`  — global MAP.md + ai/graph.json + ai/modules.md
    ///   (no arg / `kind`) — every kind whose manifest entry has `index.output`
    Index {
        #[arg(value_enum, default_value_t = IndexTarget::Kind)]
        target: IndexTarget,
    },
    /// List every known ID (debug aid).
    List,
}

#[derive(clap::ValueEnum, Clone, Copy)]
enum IndexTarget {
    Map,
    Kind,
}

// ---------------------------------------------------------------------------
// Kind manifest (loaded from ${ROKI_DOC_ROOT}/kinds.md)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ManifestFile {
    kinds: Vec<KindDef>,
}

#[derive(Debug, Deserialize, Clone)]
#[allow(dead_code)] // id_pattern + singleton are documented in kinds.md, surfaced via the manifest model for future use.
struct KindDef {
    name: String,
    #[serde(default)]
    path_globs: Vec<String>,
    /// "provides" or "generated"; absent means a normal file-backed kind.
    #[serde(default)]
    declared_via: Option<String>,
    #[serde(default)]
    id_pattern: Option<String>,
    #[serde(default)]
    singleton: bool,
    #[serde(default)]
    index: Option<KindIndex>,
}

#[derive(Debug, Deserialize, Clone)]
struct KindIndex {
    output: String,
    #[serde(default)]
    group_by: Option<String>,
}

struct Manifest {
    kinds: BTreeMap<String, KindDef>,
}

impl Manifest {
    fn load(root: &Path, doc_root: &Path) -> Result<Self> {
        let path = root.join(doc_root).join("kinds.md");
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("read kind manifest at {}", path.display()))?;
        let yaml = extract_fenced_yaml(&raw)
            .ok_or_else(|| anyhow!("{}: no ```yaml ...``` fenced block found", path.display()))?;
        let parsed: ManifestFile = serde_yaml_ng::from_str(yaml)
            .with_context(|| format!("parse kind manifest at {}", path.display()))?;
        let mut kinds = BTreeMap::new();
        for k in parsed.kinds {
            if kinds.contains_key(&k.name) {
                bail!("{}: duplicate kind `{}`", path.display(), k.name);
            }
            if k.path_globs.is_empty() && k.declared_via.is_none() {
                bail!(
                    "{}: kind `{}` has neither `path_globs` nor `declared_via`",
                    path.display(),
                    k.name
                );
            }
            kinds.insert(k.name.clone(), k);
        }
        Ok(Self { kinds })
    }

    fn knows(&self, kind: &str) -> bool {
        self.kinds.contains_key(kind)
    }
}

fn extract_fenced_yaml(content: &str) -> Option<&str> {
    // Look for the first ```yaml fenced block.
    let mut byte_start: Option<usize> = None;
    let mut byte_end: Option<usize> = None;
    let mut cursor: usize = 0;
    for line in content.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\n', '\r']);
        if byte_start.is_none() {
            if trimmed == "```yaml" || trimmed == "```yml" {
                byte_start = Some(cursor + line.len());
            }
        } else if trimmed == "```" {
            byte_end = Some(cursor);
            break;
        }
        cursor += line.len();
    }
    Some(&content[byte_start?..byte_end?])
}

// ---------------------------------------------------------------------------
// Front matter + doc model
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct FrontMatter {
    #[serde(default)]
    refs: Option<RefsBlock>,
}

#[derive(Debug, Deserialize)]
struct RefsBlock {
    id: String,
    kind: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    spec: Option<String>,
    #[serde(default)]
    provides: Vec<String>,
    #[serde(default)]
    implements: Vec<String>,
    #[serde(default)]
    depends_on: Vec<String>,
    #[serde(default)]
    related: Vec<String>,
    #[serde(default)]
    modules: Vec<String>,
    #[serde(default)]
    generated: bool,
    /// For `kind: index` — name of the kind being indexed.
    #[serde(default)]
    indexes_kind: Option<String>,
}

#[derive(Debug, Clone)]
#[allow(dead_code)] // indexes_kind is round-tripped from generated INDEX frontmatter; reserved for future use.
struct Doc {
    id: String,
    kind: String,
    title: Option<String>,
    spec: Option<String>,
    rel_path: PathBuf,
    provides: Vec<String>,
    implements: Vec<String>,
    depends_on: Vec<String>,
    related: Vec<String>,
    modules: Vec<String>,
    generated: bool,
    indexes_kind: Option<String>,
}

type EdgeMap = HashMap<String, BTreeSet<String>>;

struct Graph {
    docs: BTreeMap<String, Doc>,
    id_to_doc: HashMap<String, String>,
    forward: EdgeMap,
    reverse: EdgeMap,
    related_forward: EdgeMap,
    related_reverse: EdgeMap,
}

// ---------------------------------------------------------------------------
// Entry
// ---------------------------------------------------------------------------

fn main() -> ExitCode {
    let cli = Cli::parse();
    let root = cli
        .root
        .clone()
        .unwrap_or_else(|| std::env::current_dir().expect("cwd"));
    let doc_root = std::env::var("ROKI_DOC_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(DEFAULT_DOC_ROOT));
    match run(&cli, &root, &doc_root) {
        Ok(rc) => rc,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::from(2)
        }
    }
}

fn run(cli: &Cli, root: &Path, doc_root: &Path) -> Result<ExitCode> {
    let manifest = Manifest::load(root, doc_root)?;
    let (graph, parse_errors) = build_graph(root, doc_root, &manifest)?;
    match &cli.cmd {
        Cmd::Validate => cmd_validate(root, &manifest, &graph, &parse_errors),
        Cmd::Impact {
            ids,
            depth,
            include_related,
        } => cmd_traverse(&graph, ids, *depth, Direction::Forward, *include_related),
        Cmd::Deps {
            ids,
            depth,
            include_related,
        } => cmd_traverse(&graph, ids, *depth, Direction::Reverse, *include_related),
        Cmd::Show { id } => cmd_show(&graph, id),
        Cmd::Touched { files, no_closure } => cmd_touched(root, &graph, files, *no_closure),
        Cmd::Index { target } => cmd_index(root, doc_root, &manifest, &graph, *target),
        Cmd::List => cmd_list(&graph),
    }
}

// ---------------------------------------------------------------------------
// Scan & graph build
// ---------------------------------------------------------------------------

fn build_graph(
    root: &Path,
    doc_root: &Path,
    manifest: &Manifest,
) -> Result<(Graph, Vec<String>)> {
    let mut docs: BTreeMap<String, Doc> = BTreeMap::new();
    let mut id_to_doc: HashMap<String, String> = HashMap::new();
    let mut errors: Vec<String> = Vec::new();

    let scan_roots = derive_scan_roots(manifest, doc_root);
    let mut visited_files: BTreeSet<PathBuf> = BTreeSet::new();
    for rel in scan_roots {
        let scan = root.join(&rel);
        if !scan.exists() {
            continue;
        }
        let walker = WalkDir::new(&scan).into_iter().filter_entry(|e| {
            !e.file_name()
                .to_str()
                .map(|n| SKIP_DIRS.contains(&n))
                .unwrap_or(false)
        });
        for entry in walker.filter_map(Result::ok) {
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("md") {
                continue;
            }
            let rel = path.strip_prefix(root).unwrap_or(path).to_path_buf();
            if !visited_files.insert(rel.clone()) {
                continue;
            }

            let raw = match fs::read_to_string(path) {
                Ok(s) => s,
                Err(e) => {
                    errors.push(format!("{}: read failed: {e}", rel.display()));
                    continue;
                }
            };
            let Some(yaml) = extract_frontmatter(&raw) else {
                continue;
            };
            let fm: FrontMatter = match serde_yaml_ng::from_str(yaml) {
                Ok(v) => v,
                Err(e) => {
                    errors.push(format!("{}: front matter parse error: {e}", rel.display()));
                    continue;
                }
            };
            let Some(refs_block) = fm.refs else {
                continue;
            };
            if !manifest.knows(&refs_block.kind) {
                errors.push(format!(
                    "{} ({}): unknown kind `{}` (not declared in kinds.md)",
                    rel.display(),
                    refs_block.id,
                    refs_block.kind
                ));
                continue;
            }
            let doc = Doc {
                id: refs_block.id.clone(),
                kind: refs_block.kind.clone(),
                title: refs_block.title,
                spec: refs_block.spec,
                rel_path: rel.clone(),
                provides: refs_block.provides,
                implements: refs_block.implements,
                depends_on: refs_block.depends_on,
                related: refs_block.related,
                modules: refs_block.modules,
                generated: refs_block.generated,
                indexes_kind: refs_block.indexes_kind,
            };
            if let Some(prev) = docs.get(&doc.id) {
                errors.push(format!(
                    "duplicate id `{}` in {} and {}",
                    doc.id,
                    prev.rel_path.display(),
                    rel.display()
                ));
                continue;
            }
            for pid in &doc.provides {
                if let Some(owner) = id_to_doc.get(pid) {
                    errors.push(format!(
                        "duplicate id `{}` provided by both {} and {}",
                        pid,
                        docs.get(owner).map(|d| d.rel_path.display().to_string()).unwrap_or_default(),
                        rel.display()
                    ));
                }
                id_to_doc.insert(pid.clone(), doc.id.clone());
            }
            id_to_doc.insert(doc.id.clone(), doc.id.clone());
            docs.insert(doc.id.clone(), doc);
        }
    }

    let (forward, reverse, related_forward, related_reverse) =
        build_edges(&docs, &id_to_doc, &mut errors);
    Ok((
        Graph {
            docs,
            id_to_doc,
            forward,
            reverse,
            related_forward,
            related_reverse,
        },
        errors,
    ))
}

/// Derive scan roots from the kind manifest by stripping each `path_glob` down
/// to its longest non-glob prefix (the dir we walk). The configured doc root is
/// always included as a root so the global `map.md` can be discovered as a node.
fn derive_scan_roots(manifest: &Manifest, doc_root: &Path) -> BTreeSet<PathBuf> {
    let mut roots: BTreeSet<PathBuf> = BTreeSet::new();
    roots.insert(doc_root.to_path_buf());
    for kind in manifest.kinds.values() {
        for glob in &kind.path_globs {
            roots.insert(glob_root(glob));
        }
    }
    roots
}

fn glob_root(glob: &str) -> PathBuf {
    let meta_idx = glob.find(['*', '?', '[']);
    match meta_idx {
        None => Path::new(glob)
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from(".")),
        Some(idx) => {
            let prefix = &glob[..idx];
            let trimmed = prefix.rsplit_once('/').map(|(a, _)| a).unwrap_or("");
            if trimmed.is_empty() {
                PathBuf::from(".")
            } else {
                PathBuf::from(trimmed)
            }
        }
    }
}

fn extract_frontmatter(raw: &str) -> Option<&str> {
    let body = raw
        .strip_prefix("---\n")
        .or_else(|| raw.strip_prefix("---\r\n"))?;
    let end = body.find("\n---\n").or_else(|| body.find("\n---\r\n"))?;
    Some(&body[..end])
}

fn build_edges(
    docs: &BTreeMap<String, Doc>,
    id_to_doc: &HashMap<String, String>,
    errors: &mut Vec<String>,
) -> (EdgeMap, EdgeMap, EdgeMap, EdgeMap) {
    let mut forward: EdgeMap = HashMap::new();
    let mut reverse: EdgeMap = HashMap::new();
    let mut related_forward: EdgeMap = HashMap::new();
    let mut related_reverse: EdgeMap = HashMap::new();
    for doc in docs.values() {
        for target in &doc.related {
            if !id_to_doc.contains_key(target) {
                errors.push(format!(
                    "{} ({}): dangling reference `{}` (related)",
                    doc.rel_path.display(),
                    doc.id,
                    target
                ));
                continue;
            }
            related_forward
                .entry(doc.id.clone())
                .or_default()
                .insert(target.clone());
            related_reverse
                .entry(target.clone())
                .or_default()
                .insert(doc.id.clone());
        }
        let edges = doc.implements.iter().chain(doc.depends_on.iter());
        for target in edges {
            if !id_to_doc.contains_key(target) {
                errors.push(format!(
                    "{} ({}): dangling reference `{}`",
                    doc.rel_path.display(),
                    doc.id,
                    target
                ));
                continue;
            }
            forward.entry(doc.id.clone()).or_default().insert(target.clone());
            reverse.entry(target.clone()).or_default().insert(doc.id.clone());
        }
    }
    (forward, reverse, related_forward, related_reverse)
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

fn cmd_validate(
    root: &Path,
    _manifest: &Manifest,
    graph: &Graph,
    parse_errors: &[String],
) -> Result<ExitCode> {
    let mut errors: Vec<String> = parse_errors.to_vec();
    for doc in graph.docs.values() {
        for m in &doc.modules {
            let abs = root.join(m.trim_end_matches('/'));
            if !abs.exists() {
                errors.push(format!(
                    "{} ({}): module path `{}` does not exist",
                    doc.rel_path.display(),
                    doc.id,
                    m
                ));
            }
        }
    }
    if errors.is_empty() {
        println!("OK ({} docs)", graph.docs.len());
        Ok(ExitCode::SUCCESS)
    } else {
        for e in &errors {
            eprintln!("- {e}");
        }
        eprintln!("\n{} error(s)", errors.len());
        Ok(ExitCode::from(1))
    }
}

#[derive(Copy, Clone)]
enum Direction {
    Forward,
    Reverse,
}

fn cmd_traverse(
    graph: &Graph,
    ids: &[String],
    depth: u32,
    dir: Direction,
    include_related: bool,
) -> Result<ExitCode> {
    if ids.is_empty() {
        bail!("at least one id is required");
    }
    let mut missing = Vec::new();
    for id in ids {
        if !graph.id_to_doc.contains_key(id) {
            missing.push(id.clone());
        }
    }
    if !missing.is_empty() {
        bail!("unknown id(s): {}", missing.join(", "));
    }
    let (hard, soft) = match dir {
        Direction::Forward => (&graph.reverse, &graph.related_reverse),
        Direction::Reverse => (&graph.forward, &graph.related_forward),
    };
    let header = match dir {
        Direction::Forward => "Affected by changes to:",
        Direction::Reverse => "Dependencies of:",
    };
    println!("{header}");
    for id in ids {
        println!("  {id}");
    }
    if include_related {
        println!("(including soft `related:` edges)");
    }
    println!();

    let mut seen: BTreeSet<String> = ids.iter().cloned().collect();
    let mut frontier: VecDeque<(String, u32)> =
        ids.iter().map(|id| (id.clone(), 0)).collect();
    let mut layered: BTreeMap<u32, BTreeSet<String>> = BTreeMap::new();

    while let Some((cur, d)) = frontier.pop_front() {
        if d == depth {
            continue;
        }
        let mut neighbors: BTreeSet<String> = BTreeSet::new();
        if let Some(s) = hard.get(&cur) {
            neighbors.extend(s.iter().cloned());
        }
        if include_related {
            if let Some(s) = soft.get(&cur) {
                neighbors.extend(s.iter().cloned());
            }
        }
        for n in neighbors {
            if seen.insert(n.clone()) {
                layered.entry(d + 1).or_default().insert(n.clone());
                frontier.push_back((n.clone(), d + 1));
            }
        }
    }

    if layered.is_empty() {
        println!("(none)");
        return Ok(ExitCode::SUCCESS);
    }
    for (d, set) in &layered {
        println!("depth {d}:");
        for id in set {
            print_id_line(graph, id, "  ");
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_show(graph: &Graph, id: &str) -> Result<ExitCode> {
    let doc_id = graph
        .id_to_doc
        .get(id)
        .ok_or_else(|| anyhow!("unknown id `{id}`"))?;
    let doc = graph
        .docs
        .get(doc_id)
        .ok_or_else(|| anyhow!("internal: doc `{doc_id}` missing"))?;

    println!("id:       {}", doc.id);
    if doc.id != id {
        println!("queried:  {id}  (provided by {})", doc.id);
    }
    println!("kind:     {}", doc.kind);
    if doc.generated {
        println!("generated: true");
    }
    if let Some(s) = &doc.spec {
        println!("spec:     {s}");
    }
    if let Some(t) = &doc.title {
        println!("title:    {t}");
    }
    println!("path:     {}", doc.rel_path.display());

    print_list("provides:", &doc.provides);
    print_list("implements:", &doc.implements);
    print_list("depends_on:", &doc.depends_on);
    print_list("related:", &doc.related);
    print_list("modules:", &doc.modules);

    println!();
    println!("Direct impact (hard, who depends on this doc):");
    if let Some(rev) = graph.reverse.get(&doc.id) {
        for r in rev {
            print_id_line(graph, r, "  ");
        }
    } else {
        println!("  (none)");
    }
    println!();
    println!("Soft mentions (related: from other docs):");
    if let Some(soft) = graph.related_reverse.get(&doc.id) {
        for r in soft {
            print_id_line(graph, r, "  ");
        }
    } else {
        println!("  (none)");
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_touched(
    root: &Path,
    graph: &Graph,
    files: &[PathBuf],
    no_closure: bool,
) -> Result<ExitCode> {
    if files.is_empty() {
        bail!("at least one file is required");
    }
    let mut rel_files: Vec<String> = Vec::new();
    for f in files {
        let abs = if f.is_absolute() {
            f.clone()
        } else {
            root.join(f)
        };
        let rel = abs.strip_prefix(root).unwrap_or(f).to_string_lossy().to_string();
        rel_files.push(rel);
    }

    let mut hits: BTreeSet<String> = BTreeSet::new();
    for doc in graph.docs.values() {
        for m in &doc.modules {
            for f in &rel_files {
                if module_covers(m, f) {
                    hits.insert(doc.id.clone());
                }
            }
        }
    }

    println!("Files:");
    for f in &rel_files {
        println!("  {f}");
    }
    println!();
    if hits.is_empty() {
        println!("No docs claim these files in `modules:`.");
        return Ok(ExitCode::SUCCESS);
    }
    println!("Docs of record (modules: covers these files):");
    for id in &hits {
        print_id_line(graph, id, "  ");
    }

    if no_closure {
        return Ok(ExitCode::SUCCESS);
    }
    let mut seen = hits.clone();
    let mut frontier: VecDeque<String> = hits.iter().cloned().collect();
    let mut indirect: BTreeSet<String> = BTreeSet::new();
    while let Some(cur) = frontier.pop_front() {
        if let Some(rev) = graph.reverse.get(&cur) {
            for n in rev {
                if seen.insert(n.clone()) {
                    indirect.insert(n.clone());
                    frontier.push_back(n.clone());
                }
            }
        }
    }
    if indirect.is_empty() {
        return Ok(ExitCode::SUCCESS);
    }
    println!();
    println!("Transitively affected:");
    for id in &indirect {
        print_id_line(graph, id, "  ");
    }
    Ok(ExitCode::SUCCESS)
}

fn module_covers(pattern: &str, file: &str) -> bool {
    if let Some(prefix) = pattern.strip_suffix('/') {
        // "a/b/" matches any file *under* a/b, not a/b itself.
        file.starts_with(prefix) && file[prefix.len()..].starts_with('/')
    } else {
        file == pattern
    }
}

fn cmd_index(
    root: &Path,
    doc_root: &Path,
    manifest: &Manifest,
    graph: &Graph,
    target: IndexTarget,
) -> Result<ExitCode> {
    match target {
        IndexTarget::Map => write_map(root, doc_root, graph),
        IndexTarget::Kind => write_per_kind_indexes(root, manifest, graph),
    }
}

fn write_map(root: &Path, doc_root: &Path, graph: &Graph) -> Result<ExitCode> {
    let map_path = root.join(doc_root).join("map.md");
    let json_path = root.join(doc_root).join("ai/graph.json");
    let modules_path = root.join(doc_root).join("ai/modules.md");

    if let Some(parent) = json_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create {}", parent.display()))?;
    }

    // 1. Human-readable MAP.md
    let mut by_kind: BTreeMap<String, Vec<&Doc>> = BTreeMap::new();
    for d in graph.docs.values() {
        if d.generated {
            continue;
        }
        by_kind.entry(d.kind.clone()).or_default().push(d);
    }
    let mut map = String::new();
    map.push_str("---\nrefs:\n  id: index:map\n  kind: index\n  generated: true\n  title: \"Doc Map (all kinds)\"\n---\n\n");
    map.push_str("# Doc Map\n\n");
    map.push_str("Generated by `roki-doctools index map`. Do not edit by hand.\n");
    map.push_str("All docs across kinds. For per-kind indexes see the per-kind INDEX files; for AI consumption see [ai/graph.json](ai/graph.json).\n\n");
    for (kind, docs) in &by_kind {
        map.push_str(&format!("## {} ({})\n\n", kind, docs.len()));
        map.push_str("| ID | Title | Spec | Path |\n|---|---|---|---|\n");
        for d in docs {
            map.push_str(&format!(
                "| `{}` | {} | {} | [{}]({}) |\n",
                d.id,
                d.title.clone().unwrap_or_default(),
                d.spec.clone().unwrap_or_default(),
                d.rel_path.display(),
                doc_link(doc_root, &d.rel_path),
            ));
        }
        map.push('\n');
    }
    fs::write(&map_path, map).with_context(|| format!("write {}", map_path.display()))?;
    println!("wrote {}", map_path.display());

    // 2. AI graph.json
    let json = build_graph_json(graph);
    fs::write(&json_path, json).with_context(|| format!("write {}", json_path.display()))?;
    println!("wrote {}", json_path.display());

    // 3. ai/modules.md
    let mut by_module: BTreeMap<&String, BTreeSet<&String>> = BTreeMap::new();
    for d in graph.docs.values() {
        for m in &d.modules {
            by_module.entry(m).or_default().insert(&d.id);
        }
    }
    let mut mm = String::new();
    mm.push_str("---\nrefs:\n  id: index:modules\n  kind: index\n  generated: true\n  title: \"Source -> Doc Map\"\n---\n\n");
    mm.push_str("# Source -> Doc Map\n\n");
    mm.push_str("Generated by `roki-doctools index map`. Do not edit by hand.\n\n");
    mm.push_str("| Source path | Docs of record |\n|---|---|\n");
    for (m, ids) in &by_module {
        let cell: Vec<String> = ids.iter().map(|i| format!("`{i}`")).collect();
        mm.push_str(&format!("| `{}` | {} |\n", m, cell.join(", ")));
    }
    fs::write(&modules_path, mm)
        .with_context(|| format!("write {}", modules_path.display()))?;
    println!("wrote {}", modules_path.display());

    Ok(ExitCode::SUCCESS)
}

fn doc_link(doc_root: &Path, rel_path: &Path) -> String {
    // MAP.md lives under doc_root, so links to docs at doc_root/foo become "foo".
    if let Ok(stripped) = rel_path.strip_prefix(doc_root) {
        return stripped.display().to_string();
    }
    // Otherwise emit a path that goes up out of doc_root.
    let depth = doc_root.components().count();
    let mut up = String::new();
    for _ in 0..depth {
        up.push_str("../");
    }
    format!("{}{}", up, rel_path.display())
}

fn build_graph_json(graph: &Graph) -> String {
    // Hand-rolled JSON to avoid adding serde_json for ~80 lines.
    let mut s = String::new();
    s.push_str("{\n  \"schema_version\": 1,\n  \"docs\": [\n");
    let mut first = true;
    for d in graph.docs.values() {
        if !first {
            s.push_str(",\n");
        }
        first = false;
        s.push_str("    {");
        s.push_str(&format!("\"id\": {}", json_str(&d.id)));
        s.push_str(&format!(", \"kind\": {}", json_str(&d.kind)));
        s.push_str(&format!(", \"path\": {}", json_str(&d.rel_path.display().to_string())));
        if let Some(t) = &d.title {
            s.push_str(&format!(", \"title\": {}", json_str(t)));
        }
        if let Some(spec) = &d.spec {
            s.push_str(&format!(", \"spec\": {}", json_str(spec)));
        }
        if d.generated {
            s.push_str(", \"generated\": true");
        }
        s.push_str(&format!(", \"provides\": {}", json_arr(&d.provides)));
        s.push_str(&format!(", \"implements\": {}", json_arr(&d.implements)));
        s.push_str(&format!(", \"depends_on\": {}", json_arr(&d.depends_on)));
        s.push_str(&format!(", \"related\": {}", json_arr(&d.related)));
        s.push_str(&format!(", \"modules\": {}", json_arr(&d.modules)));
        s.push('}');
    }
    s.push_str("\n  ],\n  \"modules\": {\n");
    let mut by_module: BTreeMap<&String, BTreeSet<&String>> = BTreeMap::new();
    for d in graph.docs.values() {
        for m in &d.modules {
            by_module.entry(m).or_default().insert(&d.id);
        }
    }
    let mut first = true;
    for (m, ids) in &by_module {
        if !first {
            s.push_str(",\n");
        }
        first = false;
        let v: Vec<&String> = ids.iter().copied().collect();
        let v_str: Vec<String> = v.iter().map(|x| (*x).clone()).collect();
        s.push_str(&format!("    {}: {}", json_str(m), json_arr(&v_str)));
    }
    s.push_str("\n  }\n}\n");
    s
}

fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn json_arr(xs: &[String]) -> String {
    let parts: Vec<String> = xs.iter().map(|x| json_str(x)).collect();
    format!("[{}]", parts.join(", "))
}

fn write_per_kind_indexes(root: &Path, manifest: &Manifest, graph: &Graph) -> Result<ExitCode> {
    let mut wrote = 0u32;
    for kind in manifest.kinds.values() {
        let Some(idx) = &kind.index else {
            continue;
        };
        let mut docs: Vec<&Doc> = graph
            .docs
            .values()
            .filter(|d| d.kind == kind.name && !d.generated)
            .collect();
        docs.sort_by(|a, b| a.id.cmp(&b.id));

        let title = format!("{} Index", kind.name);
        let mut out = String::new();
        out.push_str("---\nrefs:\n");
        out.push_str(&format!("  id: index:{}\n", kind.name));
        out.push_str("  kind: index\n");
        out.push_str(&format!("  indexes_kind: {}\n", kind.name));
        out.push_str("  generated: true\n");
        out.push_str(&format!("  title: \"{title}\"\n"));
        out.push_str("---\n\n");
        out.push_str(&format!("# {title}\n\n"));
        out.push_str(&format!(
            "Generated by `roki-doctools index`. Do not edit by hand. Lists every `{}` doc in this repository.\n\n",
            kind.name
        ));

        let group_by = idx.group_by.as_deref();
        if group_by == Some("spec") {
            let mut by_spec: BTreeMap<String, Vec<&Doc>> = BTreeMap::new();
            for d in &docs {
                by_spec
                    .entry(d.spec.clone().unwrap_or_else(|| "(unspec'd)".into()))
                    .or_default()
                    .push(d);
            }
            for (spec, ds) in &by_spec {
                out.push_str(&format!("## {spec}\n\n"));
                emit_kind_table(&mut out, ds);
            }
        } else {
            emit_kind_table(&mut out, &docs);
        }

        let target = root.join(&idx.output);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        fs::write(&target, out).with_context(|| format!("write {}", target.display()))?;
        println!("wrote {}", target.display());
        wrote += 1;
    }
    if wrote == 0 {
        println!("(no kinds with `index.output` configured)");
    }
    Ok(ExitCode::SUCCESS)
}

fn emit_kind_table(out: &mut String, docs: &[&Doc]) {
    out.push_str("| ID | Title | Implements | Depends on |\n|---|---|---|---|\n");
    for d in docs {
        out.push_str(&format!(
            "| `{}` | {} | {} | {} |\n",
            d.id,
            d.title.clone().unwrap_or_default(),
            d.implements.iter().map(|s| format!("`{s}`")).collect::<Vec<_>>().join(", "),
            d.depends_on.iter().map(|s| format!("`{s}`")).collect::<Vec<_>>().join(", "),
        ));
    }
    out.push('\n');
}

fn cmd_list(graph: &Graph) -> Result<ExitCode> {
    let mut ids: Vec<&String> = graph.id_to_doc.keys().collect();
    ids.sort();
    for id in ids {
        let doc = &graph.docs[&graph.id_to_doc[id]];
        println!("{:<40} {:<14} {}", id, doc.kind, doc.rel_path.display());
    }
    Ok(ExitCode::SUCCESS)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn print_list(label: &str, xs: &[String]) {
    if xs.is_empty() {
        return;
    }
    println!("{label}");
    for x in xs {
        println!("  - {x}");
    }
}

fn print_id_line(graph: &Graph, id: &str, indent: &str) {
    if let Some(doc_id) = graph.id_to_doc.get(id) {
        if let Some(doc) = graph.docs.get(doc_id) {
            let title = doc.title.as_deref().unwrap_or("");
            println!("{indent}{id:<40} {title}");
            return;
        }
    }
    println!("{indent}{id}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_covers_exact() {
        assert!(module_covers("a/b.rs", "a/b.rs"));
        assert!(!module_covers("a/b.rs", "a/c.rs"));
    }

    #[test]
    fn module_covers_dir_prefix() {
        assert!(module_covers("a/b/", "a/b/c.rs"));
        assert!(module_covers("a/b/", "a/b/d/e.rs"));
        assert!(!module_covers("a/b/", "a/bc/x.rs"));
        assert!(!module_covers("a/b/", "a/b"));
    }

    #[test]
    fn fenced_yaml_extracted() {
        let content = "before\n```yaml\nfoo: 1\nbar: [x]\n```\nafter\n";
        let yaml = extract_fenced_yaml(content).unwrap();
        assert_eq!(yaml, "foo: 1\nbar: [x]\n");
    }

    #[test]
    fn frontmatter_extracted() {
        let content = "---\na: 1\n---\nbody\n";
        assert_eq!(extract_frontmatter(content).unwrap(), "a: 1");
    }

    #[test]
    fn frontmatter_absent() {
        assert!(extract_frontmatter("# title only\n").is_none());
    }

    #[test]
    fn glob_root_literal_path() {
        assert_eq!(glob_root(".kiro/steering/roadmap.md"), PathBuf::from(".kiro/steering"));
    }

    #[test]
    fn glob_root_with_meta_in_middle() {
        assert_eq!(glob_root(".kiro/specs/*/brief.md"), PathBuf::from(".kiro/specs"));
        assert_eq!(glob_root("docs/fr/[0-9]*.md"), PathBuf::from("docs/fr"));
        assert_eq!(glob_root("crates/*/README.md"), PathBuf::from("crates"));
    }

    #[test]
    fn glob_root_no_dir() {
        assert_eq!(glob_root("*.md"), PathBuf::from("."));
        assert_eq!(glob_root("README.md"), PathBuf::from(""));
    }

    #[test]
    fn glob_root_double_star() {
        assert_eq!(glob_root("docs/**/*.md"), PathBuf::from("docs"));
    }
}
