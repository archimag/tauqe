//! The developer-declared trust boundary of external documentation.
//!
//! A project declaration travels with the repository, so it records what the
//! project wants rather than what the developer allowed. Opening a repository
//! must therefore never let its `[[outer_docs]]` entries hand the harness a file
//! from the developer's home directory or an answer from an internal network.
//!
//! The boundary is drawn around the host, not around the network. Public
//! Internet material is public: a declared source that resolves to a public
//! address is fetched with no declaration at all, and a local source inside the
//! repository is the project's own material, read without one as well. A
//! declaration governs the two ways a repository could still reach past its own
//! material:
//!
//! - A local source outside the repository belongs to the host, so it is read
//!   only from a directory the developer declared in `local_roots`.
//! - A remote source that reaches a loopback, private, link-local, carrier-grade
//!   NAT, multicast, or otherwise non-routable address reaches the host's own
//!   network, so it requires `allow_private_network`.
//!
//! The declaration that decides this lives outside the repository, in the user
//! configuration directory (`$XDG_CONFIG_HOME/tauqe/refs-trust.toml`, or
//! `$HOME/.config/tauqe/refs-trust.toml`), because a repository's own
//! configuration would let a repository widen its own trust. One entry
//! authorizes one workspace root:
//!
//! ```text
//! [[workspace]]
//! root = "/home/dev/work/project"
//! local_roots = ["/srv/docs/upstream"]
//! allow_private_network = false
//! ```
//!
//! A workspace without a matching entry reaches no host material: it reads no
//! local source outside the repository and reaches no non-public address. That
//! is the default, and it is decided per declared document rather than per turn,
//! so a turn learns the exact reason a document stayed unavailable.
//!
//! The file keeps its historical `refs-trust.toml` spelling: it is the
//! developer's own declaration, written once outside the repository, and
//! renaming it would silently revoke a trust that was already granted.
//!
//! # Typed refusals
//!
//! Every decision this module makes is a [`TrustRefusal`], never a formatted
//! sentence. A caller can therefore tell a trust decision from a delivery
//! failure, and every refusal carries both the precise cause and the
//! ready-to-paste [`WorkspaceDeclaration`] that would authorize the source. The
//! refusal is addressed to the developer, because no retry inside a turn can
//! widen a declaration the repository cannot reach.
//!
//! # Remote targets
//!
//! A remote source is constrained by address rather than by name. A public
//! address needs no entry, so published material is fetched without asking the
//! developer to pre-authorize a repository. The harness still refuses an address
//! that belongs to a loopback, private, link-local, carrier-grade NAT, multicast,
//! or otherwise non-routable block, pins the address it validated into the
//! request, and revalidates every redirect hop instead of letting the HTTP client
//! follow it. A URL that carries credentials is refused, because the harness
//! would send them to a host the source itself named. `allow_private_network` is
//! the single escape hatch, and it belongs to the workspace entry, never to the
//! repository. Resolving a name is delivery work and lives beside the transfer,
//! so this module only classifies the addresses it is handed. An IPv6 address
//! that embeds an IPv4 address, whether in the mapped `::ffff:a.b.c.d` form or
//! in the deprecated compatible `::a.b.c.d` form, is classified by the address
//! it embeds, so neither spelling reaches a block a plain IPv4 URL cannot.

use std::io::ErrorKind;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::{Path, PathBuf};

use reqwest::Url;
use serde::{Deserialize, Serialize};

use crate::config::paths::{dirs_config, dirs_home};

/// File name of the trust declaration inside the user configuration directory.
const TRUST_FILE_NAME: &str = "refs-trust.toml";

/// The `[[workspace]]` entry a developer writes to authorize one workspace.
///
/// The entry has two readers: it is the shape read from `refs-trust.toml`, and
/// it is the ready-to-paste remedy a [`TrustRefusal`] carries. One declaration
/// therefore both parses the developer's file and reconstructs the exact block a
/// refusal asks for, instead of leaving a formatted sentence that someone has to
/// translate back into TOML.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceDeclaration {
    /// Absolute path of the repository this entry authorizes.
    pub root: String,
    /// Absolute directories a declared local source may be read from.
    #[serde(default)]
    pub local_roots: Vec<String>,
    /// Whether a declared remote source may address a non-public network.
    #[serde(default)]
    pub allow_private_network: bool,
}

impl WorkspaceDeclaration {
    /// Renders the entry as a copyable `[[workspace]]` block.
    pub fn render(&self) -> String {
        #[derive(Serialize)]
        struct Declaration<'a> {
            workspace: Vec<&'a WorkspaceDeclaration>,
        }

        match toml::to_string(&Declaration {
            workspace: vec![self],
        }) {
            Ok(block) => block.trim_end().to_string(),
            // The entry holds a string, a list of strings, and a boolean, so
            // serialization cannot fail. A refusal still never loses its cause
            // to a formatting accident.
            Err(error) => format!("# the workspace declaration could not be rendered: {error}"),
        }
    }
}

/// A refusal the developer's trust declaration issues for one declared source.
///
/// A trust refusal is not a delivery failure: no retry inside a turn can satisfy
/// it, because the decision lives in a declaration the repository cannot reach.
/// Every refusal carries the precise cause and, wherever a declaration can
/// authorize the source, the workspace entry that would do it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrustRefusal {
    /// The workspace has no entry in the trust declaration, so it reaches no
    /// host material: no local source outside the repository and no non-public
    /// address.
    WorkspaceNotAuthorized {
        /// The entry that authorizes the workspace and the request it refused.
        /// It carries `allow_private_network` when the source needed a
        /// non-public network.
        workspace: WorkspaceDeclaration,
    },
    /// A declared local source lies outside every trust root of the workspace.
    LocalSourceOutsideRoots {
        /// Path the declaration names, as written.
        source: String,
        /// Canonical path the source resolves to.
        resolved: PathBuf,
        /// The entry that authorizes the workspace and the source's directory.
        workspace: WorkspaceDeclaration,
    },
    /// A declared source carries credentials the harness would hand to its host.
    CredentialsInUrl {
        /// The declared address.
        url: String,
        /// The entry that authorizes the workspace.
        workspace: WorkspaceDeclaration,
    },
    /// A declared source reaches a network that is not the public Internet.
    NonPublicNetwork {
        /// The declared address.
        url: String,
        /// The address the harness refused.
        address: IpAddr,
        /// The entry that authorizes the workspace on that network.
        workspace: WorkspaceDeclaration,
    },
    /// A declared source is not an address the registry can fetch at all.
    UnsupportedTarget {
        /// The declared address.
        url: String,
        /// What the address carries instead of a fetchable location.
        problem: String,
    },
}

impl TrustRefusal {
    /// Exact cause of the refusal, in the vocabulary of the declared source.
    pub fn reason(&self) -> String {
        match self {
            Self::WorkspaceNotAuthorized { workspace } => format!(
                "the workspace `{root}` has no entry in `{TRUST_FILE_NAME}`, so it reads no local source outside the repository and reaches no non-public address",
                root = workspace.root
            ),
            Self::LocalSourceOutsideRoots {
                source, resolved, ..
            } => format!(
                "the local source `{source}` resolves to `{resolved}`, which lies outside every trust root this workspace declares, so the document was not read",
                resolved = resolved.display()
            ),
            Self::CredentialsInUrl { url, .. } => format!(
                "the source `{url}` carries credentials in the URL, so it was refused instead of being sent to a host the declaration chose"
            ),
            Self::NonPublicNetwork { url, address, .. } => format!(
                "the source `{url}` reaches the non-public address `{address}`, so the request was refused"
            ),
            Self::UnsupportedTarget { url, problem } => {
                format!("the source `{url}` is not an address the registry can fetch: it {problem}")
            }
        }
    }

    /// Ready-to-paste `[[workspace]]` declaration that authorizes the source.
    ///
    /// A source the harness will simply not fetch has no such remedy: the
    /// `[[outer_docs]]` declaration itself must name a different source.
    pub fn remedy(&self) -> Option<String> {
        let workspace = match self {
            Self::WorkspaceNotAuthorized { workspace }
            | Self::LocalSourceOutsideRoots { workspace, .. }
            | Self::CredentialsInUrl { workspace, .. }
            | Self::NonPublicNetwork { workspace, .. } => workspace,
            Self::UnsupportedTarget { .. } => return None,
        };
        let block = workspace.render();
        match self {
            // No trust declaration can strip credentials from a declared
            // source, so the block carries the edit the developer owes beside
            // the entry it authorizes.
            Self::CredentialsInUrl { url, .. } => Some(format!(
                "# Remove the credentials from the declared source `{url}`.\n{block}"
            )),
            _ => Some(block),
        }
    }
}

/// The trust a developer grants the external documentation of one workspace.
///
/// It answers two questions and nothing else: which local directories outside the
/// repository a declared source may be read from, and whether a declared host may
/// address a network that is not the public Internet. A workspace no entry
/// authorizes answers neither, which is the state a repository has by default, and
/// it still reads public Internet material together with its own files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RefsTrust {
    /// Canonical repository root this declaration applies to.
    workspace: PathBuf,
    /// Whether the user file carries an entry for `workspace`.
    authorized: bool,
    /// Canonical directories a declared local source may be read from.
    local_roots: Vec<PathBuf>,
    /// Whether a declared remote source may address a non-public network.
    allow_private_network: bool,
}

impl RefsTrust {
    /// The declaration of a workspace no entry authorizes.
    pub(crate) fn unauthorized(repo_root: &Path) -> Self {
        Self {
            workspace: canonical(repo_root),
            authorized: false,
            local_roots: Vec::new(),
            allow_private_network: false,
        }
    }

    /// Builds an authorizing declaration from its parts.
    #[cfg(test)]
    pub(crate) fn for_workspace(
        workspace: &Path,
        local_roots: Vec<PathBuf>,
        allow_private_network: bool,
    ) -> Self {
        Self {
            workspace: canonical(workspace),
            authorized: true,
            local_roots: local_roots
                .into_iter()
                .map(|root| canonical(&root))
                .collect(),
            allow_private_network,
        }
    }

    /// The declaration that reproduces this workspace exactly as it stands.
    fn granted(&self) -> WorkspaceDeclaration {
        WorkspaceDeclaration {
            root: self.workspace.to_string_lossy().into_owned(),
            local_roots: self
                .local_roots
                .iter()
                .map(|root| root.to_string_lossy().into_owned())
                .collect(),
            allow_private_network: self.allow_private_network,
        }
    }

    /// The declaration that authorizes this workspace and one more directory.
    fn granting_root(&self, directory: &Path) -> WorkspaceDeclaration {
        let mut declaration = self.granted();
        let directory = directory.to_string_lossy().into_owned();
        if !declaration.local_roots.contains(&directory) {
            declaration.local_roots.push(directory);
        }
        declaration
    }

    /// The declaration that authorizes this workspace on a non-public network.
    fn granting_network(&self) -> WorkspaceDeclaration {
        WorkspaceDeclaration {
            allow_private_network: true,
            ..self.granted()
        }
    }

    /// Reports whether a declared local source may be read.
    ///
    /// A source inside the repository is the project's own material, published to
    /// the harness together with the declaration that cites it, so it is read
    /// without an entry. Every other source belongs to the host and requires one.
    ///
    /// The comparison runs over canonical paths, so a symlink placed inside a
    /// declared root cannot carry the read outside it. A source that does not
    /// exist yet is resolved through its canonical parent, so a missing file is
    /// still answered by the read itself instead of by the trust rule.
    pub(crate) fn permits_local_source(&self, source: &str) -> Result<(), TrustRefusal> {
        let resolved = resolve_local_source(source);
        if resolved.starts_with(&self.workspace) {
            return Ok(());
        }
        if !self.authorized {
            return Err(TrustRefusal::WorkspaceNotAuthorized {
                workspace: self.granted(),
            });
        }
        if self
            .local_roots
            .iter()
            .any(|root| resolved.starts_with(root))
        {
            return Ok(());
        }
        let directory = resolved
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| resolved.clone());
        Err(TrustRefusal::LocalSourceOutsideRoots {
            workspace: self.granting_root(&directory),
            source: source.to_string(),
            resolved,
        })
    }

    /// Validates the scheme, the credentials, and the literal address of `url`.
    ///
    /// A hostname is classified by [`Self::check_remote_address`] once the
    /// transfer has resolved it, because only the resolved addresses say which
    /// network the request reaches. A public address therefore needs no entry,
    /// and the declaration decides only what the address turns out to be.
    pub(crate) fn check_remote_url(&self, url: &Url) -> Result<(), TrustRefusal> {
        let scheme = url.scheme();
        if scheme != "http" && scheme != "https" {
            return Err(TrustRefusal::UnsupportedTarget {
                url: url.to_string(),
                problem: format!("uses the scheme `{scheme}`, which the registry does not fetch"),
            });
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(TrustRefusal::CredentialsInUrl {
                url: url.to_string(),
                workspace: self.granted(),
            });
        }
        if url.host_str().is_none() {
            return Err(TrustRefusal::UnsupportedTarget {
                url: url.to_string(),
                problem: "names no host".to_string(),
            });
        }
        if let Some(address) = literal_address(url) {
            self.check_remote_address(url, address)?;
        }
        Ok(())
    }

    /// Refuses an address that does not belong to the public Internet.
    ///
    /// A public address is public material and needs no entry. A non-public
    /// address reaches the host's own network, so it requires a workspace entry
    /// that carries `allow_private_network`, and a workspace without an entry at
    /// all cannot reach it either.
    pub(crate) fn check_remote_address(
        &self,
        url: &Url,
        address: IpAddr,
    ) -> Result<(), TrustRefusal> {
        if is_public_address(address) || self.allow_private_network {
            return Ok(());
        }
        if !self.authorized {
            return Err(TrustRefusal::WorkspaceNotAuthorized {
                workspace: self.granting_network(),
            });
        }
        Err(TrustRefusal::NonPublicNetwork {
            url: url.to_string(),
            address,
            workspace: self.granting_network(),
        })
    }
}

/// Reads the trust declaration that applies to `repo_root`.
///
/// Reading the declaration is blocking file-system work, so it runs off the
/// async executor thread. A missing declaration trusts nothing, which is a
/// legitimate state: a repository materializes its external documentation only
/// after its developer allowed it.
pub(crate) fn load_trust(repo_root: &Path) -> RefsTrust {
    crate::blocking::run_blocking(|| trust_at(repo_root))
}

/// Resolves the trust declaration of `repo_root`.
fn trust_at(repo_root: &Path) -> RefsTrust {
    let Some(path) = trust_file_path() else {
        return RefsTrust::unauthorized(repo_root);
    };
    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return RefsTrust::unauthorized(repo_root)
        }
        Err(error) => {
            tracing::warn!(
                path = %path.display(),
                error = %error,
                "the documentation trust declaration could not be read"
            );
            return RefsTrust::unauthorized(repo_root);
        }
    };
    match toml::from_str::<TrustDeclaration>(&content) {
        Ok(declaration) => declaration.resolve(repo_root),
        Err(error) => {
            tracing::warn!(
                path = %path.display(),
                error = %error,
                "the documentation trust declaration could not be parsed"
            );
            RefsTrust::unauthorized(repo_root)
        }
    }
}

/// Resolves the user-level declaration path.
///
/// The declaration lives in the user configuration directory, which no
/// repository reaches through the harness: `tauqe.toml` travels with the
/// repository and would let a repository authorize itself.
fn trust_file_path() -> Option<PathBuf> {
    let base = dirs_config().or_else(|| dirs_home().map(|home| home.join(".config")))?;
    Some(base.join("tauqe").join(TRUST_FILE_NAME))
}

/// The user-file shape read from `refs-trust.toml`.
#[derive(Deserialize)]
struct TrustDeclaration {
    /// Authorized workspaces, in declaration order.
    #[serde(default)]
    workspace: Vec<WorkspaceDeclaration>,
}

impl TrustDeclaration {
    /// Resolves the entry that authorizes `repo_root`.
    ///
    /// A relative root is skipped instead of being resolved against the working
    /// directory: an entry that does not name an absolute location cannot be
    /// compared with a repository, and guessing would authorize the wrong one.
    fn resolve(&self, repo_root: &Path) -> RefsTrust {
        let wanted = canonical(repo_root);
        let workspace = self.workspace.iter().find(|workspace| {
            let root = Path::new(&workspace.root);
            root.is_absolute() && canonical(root) == wanted
        });
        let Some(workspace) = workspace else {
            return RefsTrust::unauthorized(repo_root);
        };
        RefsTrust {
            workspace: wanted,
            authorized: true,
            local_roots: workspace
                .local_roots
                .iter()
                .map(PathBuf::from)
                .filter(|root| root.is_absolute())
                .map(|root| canonical(&root))
                .collect(),
            allow_private_network: workspace.allow_private_network,
        }
    }
}

/// Canonicalizes `path` when the platform can, and keeps it as written when it
/// cannot: a directory that does not exist yet still names the place a later
/// source will live in.
fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Resolves a declared local source to the path the trust rule compares.
///
/// A source that exists is canonicalized as a whole. A source that does not
/// exist is resolved through its canonical parent, so a missing file below a
/// symlinked directory is still compared inside the directory it really names.
fn resolve_local_source(source: &str) -> PathBuf {
    if let Ok(resolved) = std::fs::canonicalize(source) {
        return resolved;
    }
    let written = PathBuf::from(source);
    match (written.parent(), written.file_name()) {
        (Some(parent), Some(name)) => canonical(parent).join(name),
        _ => written,
    }
}

/// Returns the literal address `url` names, when it names one.
pub(crate) fn literal_address(url: &Url) -> Option<IpAddr> {
    let host = url.host_str()?;
    let bare = host
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or(host);
    bare.parse::<IpAddr>().ok()
}

/// Reports whether an address belongs to the public Internet.
fn is_public_address(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(ip) => is_public_v4(ip),
        IpAddr::V6(ip) => is_public_v6(ip),
    }
}

/// Reports whether an IPv4 address belongs to the public Internet.
fn is_public_v4(address: Ipv4Addr) -> bool {
    if address.is_private()
        || address.is_loopback()
        || address.is_link_local()
        || address.is_broadcast()
        || address.is_documentation()
        || address.is_unspecified()
        || address.is_multicast()
    {
        return false;
    }
    // The remaining reserved blocks: `0.0.0.0/8` names this network,
    // `100.64.0.0/10` is the carrier-grade NAT block, and `240.0.0.0/4` is
    // reserved for future use. None of them is reachable from the Internet, and
    // the standard library does not classify them.
    let octets = address.octets();
    octets[0] != 0 && !(octets[0] == 100 && (64..=127).contains(&octets[1])) && octets[0] < 240
}

/// Returns the IPv4 address the deprecated `::a.b.c.d` form embeds.
///
/// The compatible form carries the address in the same low 32 bits as the
/// mapped `::ffff:a.b.c.d` form, but `Ipv6Addr::to_ipv4_mapped` reads only the
/// mapped one, and the general `Ipv6Addr::to_ipv4` that read both is
/// deprecated. The classification therefore reads the octets directly, so a
/// stack that still honors the compatible form cannot reach a host this module
/// never classified.
fn compatible_ipv4(address: Ipv6Addr) -> Option<Ipv4Addr> {
    let [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, a, b, c, d] = address.octets() else {
        return None;
    };
    Some(Ipv4Addr::new(a, b, c, d))
}

/// Reports whether an IPv6 address belongs to the public Internet.
fn is_public_v6(address: Ipv6Addr) -> bool {
    if let Some(mapped) = address.to_ipv4_mapped() {
        return is_public_v4(mapped);
    }
    if let Some(compatible) = compatible_ipv4(address) {
        return is_public_v4(compatible);
    }
    if address.is_unspecified() || address.is_loopback() || address.is_multicast() {
        return false;
    }
    let segments = address.segments();
    // `fc00::/7` is the unique-local block and `fe80::/10` the link-local block,
    // the two roles IPv4 gives to `10.0.0.0/8` and `169.254.0.0/16`.
    // `2001:db8::/32` is reserved for documentation, and `64:ff9b::/96` is the
    // well-known NAT64 prefix whose low 32 bits embed an IPv4 address a
    // translator would actually reach.
    let unique_local = (segments[0] & 0xfe00) == 0xfc00;
    let link_local = (segments[0] & 0xffc0) == 0xfe80;
    let documentation = segments[0] == 0x2001 && segments[1] == 0x0db8;
    let nat64 = segments[0] == 0x0064
        && segments[1] == 0xff9b
        && segments[2..6].iter().all(|segment| *segment == 0);
    !(unique_local || link_local || documentation || nat64)
}

#[cfg(test)]
mod tests {
    use super::{is_public_address, RefsTrust, TrustDeclaration, TrustRefusal};
    use std::net::IpAddr;
    use std::path::{Path, PathBuf};
    use tempfile::tempdir;

    /// The declaration a developer would write for one workspace.
    const DECLARATION: &str = r#"[[workspace]]
root = "/work/project"
local_roots = ["/srv/docs"]
allow_private_network = true
"#;

    /// Parses a declaration the way the user file is parsed.
    fn declaration(content: &str) -> TrustDeclaration {
        toml::from_str(content).expect("the declaration parses")
    }

    #[test]
    fn test_a_workspace_entry_authorizes_its_own_declaration() {
        let trust = declaration(DECLARATION).resolve(Path::new("/work/project"));
        assert!(trust.authorized);
        assert!(trust.allow_private_network);
        assert_eq!(trust.local_roots.len(), 1);
    }

    #[test]
    fn test_a_repository_without_an_entry_trusts_nothing() {
        let trust = declaration(DECLARATION).resolve(Path::new("/work/other"));
        assert_eq!(trust, RefsTrust::unauthorized(Path::new("/work/other")));
        assert!(!trust.authorized);
        assert!(!trust.allow_private_network);
        assert!(trust.local_roots.is_empty());
    }

    #[test]
    fn test_a_relative_root_is_never_interpreted() {
        let content = r#"[[workspace]]
root = "project"
local_roots = ["/srv/docs"]
"#;
        let trust = declaration(content).resolve(Path::new("project"));
        assert!(!trust.authorized);
        assert!(trust.local_roots.is_empty());
    }

    #[test]
    fn test_a_source_inside_a_declared_root_is_permitted() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("notes.md");
        std::fs::write(&source, "notes").unwrap();

        let trust = RefsTrust::for_workspace(dir.path(), vec![dir.path().to_path_buf()], false);
        assert!(trust
            .permits_local_source(&source.to_string_lossy())
            .is_ok());
    }

    #[test]
    fn test_an_unauthorized_workspace_reads_no_host_material() {
        let trust = RefsTrust::unauthorized(Path::new("/work/project"));

        let local = trust
            .permits_local_source("/srv/docs/notes.md")
            .expect_err("an unauthorized workspace reads no local source");
        assert!(matches!(local, TrustRefusal::WorkspaceNotAuthorized { .. }));
        assert!(local.reason().contains("no entry"), "{local:?}");
        let remedy = local.remedy().expect("the refusal carries its declaration");
        assert!(
            remedy.contains("[[workspace]]"),
            "unexpected remedy: {remedy}"
        );
        assert!(
            remedy.contains("/work/project"),
            "unexpected remedy: {remedy}"
        );

        let private = reqwest::Url::parse("http://127.0.0.1:8080/llm.txt").unwrap();
        let refusal = trust
            .check_remote_url(&private)
            .expect_err("an unauthorized workspace reaches no non-public address");
        assert!(matches!(
            refusal,
            TrustRefusal::WorkspaceNotAuthorized { .. }
        ));
        let remedy = refusal
            .remedy()
            .expect("the refusal carries its declaration");
        assert!(
            remedy.contains("allow_private_network = true"),
            "unexpected remedy: {remedy}"
        );

        let public = reqwest::Url::parse("https://example.com/llm.txt").unwrap();
        assert!(
            trust.check_remote_url(&public).is_ok(),
            "the public Internet needs no declaration"
        );
    }

    #[test]
    fn test_a_public_address_needs_no_declaration() {
        let trust = RefsTrust::unauthorized(Path::new("/work/project"));
        for address in [
            "https://example.com/llm.txt",
            "https://93.184.216.34/llm.txt",
            "https://[2606:4700:4700::1111]/llm.txt",
        ] {
            let url = reqwest::Url::parse(address).unwrap();
            assert!(
                trust.check_remote_url(&url).is_ok(),
                "{address} must be fetched without a declaration"
            );
        }
    }

    #[test]
    fn test_a_source_inside_the_repository_is_read_without_an_entry() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("api.md");
        std::fs::write(&source, "endpoint notes").unwrap();

        let trust = RefsTrust::unauthorized(dir.path());
        assert!(
            trust
                .permits_local_source(&source.to_string_lossy())
                .is_ok(),
            "the project's own files need no entry"
        );
    }

    #[test]
    fn test_a_source_outside_every_declared_root_is_refused() {
        let allowed = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let source = outside.path().join("id_ed25519");
        std::fs::write(&source, "private key").unwrap();

        let trust =
            RefsTrust::for_workspace(allowed.path(), vec![allowed.path().to_path_buf()], false);
        let refusal = trust
            .permits_local_source(&source.to_string_lossy())
            .expect_err("a source outside every root is refused");

        match &refusal {
            TrustRefusal::LocalSourceOutsideRoots {
                workspace,
                resolved,
                ..
            } => {
                let directory = std::fs::canonicalize(outside.path()).unwrap();
                assert!(resolved.starts_with(&directory));
                assert!(
                    workspace
                        .local_roots
                        .iter()
                        .any(|root| Path::new(root).starts_with(&directory)),
                    "the remedy must name the refused directory: {workspace:?}"
                );
            }
            other => panic!("expected a local-root refusal, got {other:?}"),
        }
        assert!(refusal.reason().contains("trust root"), "{refusal:?}");
    }

    #[cfg(unix)]
    #[test]
    fn test_a_symlink_cannot_carry_the_read_outside_a_declared_root() {
        let allowed = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let secret = outside.path().join("secret.md");
        std::fs::write(&secret, "secret").unwrap();
        let link = allowed.path().join("link.md");
        std::os::unix::fs::symlink(&secret, &link).unwrap();

        let trust =
            RefsTrust::for_workspace(allowed.path(), vec![allowed.path().to_path_buf()], false);
        let refusal = trust
            .permits_local_source(&link.to_string_lossy())
            .expect_err("the symlink target decides, not the link");
        assert!(
            matches!(refusal, TrustRefusal::LocalSourceOutsideRoots { .. }),
            "unexpected classification: {refusal:?}"
        );
    }

    #[test]
    fn test_a_non_public_target_needs_the_declaration() {
        let url = reqwest::Url::parse("http://127.0.0.1:8080/llm.txt").unwrap();
        let closed = RefsTrust::for_workspace(Path::new("/work/project"), Vec::new(), false);
        let refusal = closed
            .check_remote_url(&url)
            .expect_err("a loopback target needs the declaration");
        assert!(
            matches!(refusal, TrustRefusal::NonPublicNetwork { .. }),
            "unexpected classification: {refusal:?}"
        );
        assert!(
            refusal.reason().contains("non-public address"),
            "unexpected reason: {}",
            refusal.reason()
        );
        let remedy = refusal
            .remedy()
            .expect("the refusal carries its declaration");
        assert!(
            remedy.contains("allow_private_network = true"),
            "unexpected remedy: {remedy}"
        );

        let open = RefsTrust::for_workspace(Path::new("/work/project"), Vec::new(), true);
        assert!(open.check_remote_url(&url).is_ok());
    }

    #[test]
    fn test_an_ipv4_compatible_target_needs_the_declaration() {
        let url = reqwest::Url::parse("http://[::127.0.0.1]:8080/llm.txt").unwrap();
        let closed = RefsTrust::for_workspace(Path::new("/work/project"), Vec::new(), false);
        let refusal = closed
            .check_remote_url(&url)
            .expect_err("a compatible loopback target needs the declaration");
        assert!(
            matches!(refusal, TrustRefusal::NonPublicNetwork { .. }),
            "unexpected classification: {refusal:?}"
        );
        assert!(
            refusal.reason().contains("non-public address"),
            "unexpected reason: {}",
            refusal.reason()
        );

        let open = RefsTrust::for_workspace(Path::new("/work/project"), Vec::new(), true);
        assert!(open.check_remote_url(&url).is_ok());
    }
    #[test]
    fn test_a_url_carrying_credentials_is_refused() {
        let url = reqwest::Url::parse("https://user:secret@example.com/llm.txt").unwrap();
        let trust = RefsTrust::for_workspace(Path::new("/work/project"), Vec::new(), true);
        let refusal = trust
            .check_remote_url(&url)
            .expect_err("credentials in the URL are refused");
        assert!(
            matches!(refusal, TrustRefusal::CredentialsInUrl { .. }),
            "unexpected classification: {refusal:?}"
        );
        assert!(
            refusal.reason().contains("credentials"),
            "unexpected reason: {}",
            refusal.reason()
        );
        let remedy = refusal
            .remedy()
            .expect("the refusal carries its declaration");
        assert!(remedy.contains("Remove the credentials"), "{remedy}");
        assert!(remedy.contains("[[workspace]]"), "{remedy}");
    }

    #[test]
    fn test_a_scheme_outside_http_is_refused_without_a_declaration() {
        let url = reqwest::Url::parse("file:///etc/passwd").unwrap();
        let trust = RefsTrust::for_workspace(Path::new("/work/project"), Vec::new(), true);
        let refusal = trust
            .check_remote_url(&url)
            .expect_err("the registry fetches only http and https");
        assert!(
            matches!(refusal, TrustRefusal::UnsupportedTarget { .. }),
            "unexpected classification: {refusal:?}"
        );
        assert!(
            refusal.reason().contains("scheme"),
            "unexpected reason: {}",
            refusal.reason()
        );
        assert!(
            refusal.remedy().is_none(),
            "no trust entry makes the harness fetch a local scheme"
        );
    }

    #[test]
    fn test_a_remedy_renders_a_copyable_workspace_block() {
        let trust = RefsTrust::for_workspace(Path::new("/work/project"), Vec::new(), false);
        let url = reqwest::Url::parse("http://10.1.2.3/llm.txt").unwrap();
        let refusal = trust
            .check_remote_address(&url, "10.1.2.3".parse().unwrap())
            .expect_err("a private address needs the declaration");

        let remedy = refusal
            .remedy()
            .expect("the refusal carries its declaration");
        assert!(remedy.starts_with("[[workspace]]"), "got: {remedy}");
        assert!(remedy.contains("root = \"/work/project\""), "got: {remedy}");
        assert!(remedy.contains("local_roots = []"), "got: {remedy}");
        assert!(
            remedy.contains("allow_private_network = true"),
            "got: {remedy}"
        );

        // The rendered block parses back into the declaration it describes, so
        // copying it into the trust file really authorizes the workspace.
        let parsed = declaration(&remedy);
        assert_eq!(parsed.workspace.len(), 1);
        assert_eq!(parsed.workspace[0].root, "/work/project");
        assert!(parsed.workspace[0].allow_private_network);
    }

    #[test]
    fn test_a_declared_local_root_is_stored_canonically() {
        let dir = tempdir().unwrap();
        let trust = RefsTrust::for_workspace(dir.path(), vec![dir.path().to_path_buf()], false);
        let expected: PathBuf = std::fs::canonicalize(dir.path()).unwrap();
        assert_eq!(trust.local_roots, vec![expected]);
    }

    #[test]
    fn test_non_public_addresses_are_classified() {
        for address in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.5.4",
            "192.168.0.7",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "255.255.255.255",
            "224.0.0.1",
            "240.0.0.1",
            "192.0.2.10",
            "::1",
            "::",
            "fc00::1",
            "fd12:3456::1",
            "fe80::1",
            "ff02::1",
            "2001:db8::1",
            "64:ff9b::7f00:1",
            "::ffff:127.0.0.1",
            "::127.0.0.1",
            "::10.1.2.3",
            "::169.254.169.254",
            "::192.168.0.7",
        ] {
            let ip: IpAddr = address.parse().unwrap();
            assert!(!is_public_address(ip), "{address} must not count as public");
        }
    }

    #[test]
    fn test_public_addresses_are_classified() {
        for address in [
            "1.1.1.1",
            "8.8.8.8",
            "93.184.216.34",
            "2606:4700:4700::1111",
            "::93.184.216.34",
        ] {
            let ip: IpAddr = address.parse().unwrap();
            assert!(is_public_address(ip), "{address} must count as public");
        }
    }
}
