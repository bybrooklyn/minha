//! Self-contained, versioned book/library infrastructure.
//!
//! The library deliberately keeps the model-facing surface small: metadata and
//! retrieval records are typed, while the catalog remains an in-memory index.
//! The bundled registry is compile-trusted: its `builtin:` key and signature
//! values are provenance markers, not Ed25519 signatures. External signature
//! verification belongs to a registry boundary and is intentionally out of scope here.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use thiserror::Error;

pub const BOOK_SCHEMA_VERSION: u16 = 2;
pub const BUNDLED_PACK_COUNT: usize = 10;
pub const MIN_BUNDLED_ENTRY_COUNT: usize = 100;
pub const BUILTIN_BOOK_KEY_ID: &str = "builtin:minha-books-v2";
pub const BUILTIN_BOOK_SIGNATURE: &str = "builtin:sha256-content-digest-v2";
pub const BUNDLED_MANIFEST: &str = include_str!("../../../bundled/books/manifest.json");

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustState {
    #[default]
    Unverified,
    Draft,
    Verified,
    Promoted,
    Stale,
    Rejected,
}

impl TrustState {
    pub const fn searchable(self) -> bool {
        matches!(self, Self::Verified | Self::Promoted | Self::Stale)
    }
    pub const fn rank(self) -> u8 {
        match self {
            Self::Promoted => 3,
            Self::Verified => 2,
            Self::Stale => 1,
            _ => 0,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Taxonomy {
    Programming,
    Systems,
    Data,
    Security,
    Product,
    Design,
    Management,
    Research,
    Writing,
    Operations,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Language {
    English,
    Spanish,
    French,
    German,
    Japanese,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Freshness {
    #[default]
    Unknown,
    Current,
    ReviewDue,
    Stale,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct Staleness {
    #[serde(default)]
    pub status: Freshness,
    #[serde(default)]
    pub checked_at: Option<String>,
    #[serde(default)]
    pub review_after: Option<String>,
    #[serde(default)]
    pub reason: String,
}

impl Staleness {
    pub fn current() -> Self {
        Self {
            status: Freshness::Current,
            checked_at: Some("2026-07-29".into()),
            review_after: Some("2027-01-29".into()),
            reason: "Curated against the listed source metadata; recheck before relying on time-sensitive details.".into(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TokenBudget {
    #[serde(default)]
    pub index_tokens: u32,
    #[serde(default)]
    pub compact_tokens: u32,
    #[serde(default)]
    pub detailed_tokens: u32,
}

impl Default for TokenBudget {
    fn default() -> Self {
        Self {
            index_tokens: 160,
            compact_tokens: 900,
            detailed_tokens: 2_400,
        }
    }
}

impl TokenBudget {
    pub fn valid(&self) -> bool {
        self.index_tokens > 0
            && self.index_tokens <= self.compact_tokens
            && self.compact_tokens <= self.detailed_tokens
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    #[default]
    CuratedEditorial,
    OfficialDocumentation,
    Standard,
    ResearchPaper,
    Book,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct SourceMetadata {
    #[serde(default)]
    pub kind: SourceKind,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub publisher: String,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub accessed: Option<String>,
    #[serde(default)]
    pub license: Option<String>,
    #[serde(default)]
    pub note: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelTier {
    Start,
    Spark,
    Larger,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RetrievalBudget {
    pub input_tokens: u32,
    pub output_tokens: u32,
}

impl RetrievalBudget {
    pub const START: Self = Self {
        input_tokens: 4_000,
        output_tokens: 1_000,
    };
    pub const SPARK: Self = Self {
        input_tokens: 16_000,
        output_tokens: 4_000,
    };
    pub const LARGER: Self = Self {
        input_tokens: 32_000,
        output_tokens: 8_000,
    };

    pub const fn for_tier(tier: ModelTier) -> Self {
        match tier {
            ModelTier::Start => Self::START,
            ModelTier::Spark => Self::SPARK,
            ModelTier::Larger => Self::LARGER,
        }
    }
    pub fn for_model(model: &str) -> Self {
        match model {
            "spark" | "gpt-5.3-codex-spark" => Self::SPARK,
            "larger" | "luna" | "terra" | "sol" => Self::LARGER,
            _ => Self::START,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct BookMetadata {
    pub schema_version: u16,
    pub id: String,
    pub version: String,
    pub title: String,
    pub authors: Vec<String>,
    pub language: Language,
    pub taxonomy: Vec<Taxonomy>,
    pub tags: Vec<String>,
    pub path: String,
    pub abstract_text: String,
    #[serde(default)]
    pub trust: TrustState,
    #[serde(default)]
    pub staleness: Staleness,
    #[serde(default)]
    pub token_budget: TokenBudget,
    #[serde(default)]
    pub source: SourceMetadata,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Chapter {
    pub id: String,
    pub title: String,
    pub summary: String,
    pub sections: Vec<Section>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Section {
    pub id: String,
    pub title: String,
    pub summary: String,
    pub key_facts: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct KeyFact {
    pub id: String,
    pub statement: String,
    pub tags: Vec<String>,
    pub citation_ids: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Citation {
    pub id: String,
    pub locator: String,
    pub source: String,
    pub note: String,
    #[serde(default)]
    pub kind: SourceKind,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub accessed: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Book {
    pub metadata: BookMetadata,
    pub chapters: Vec<Chapter>,
    pub key_facts: Vec<KeyFact>,
    pub citations: Vec<Citation>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ManifestPack {
    pub id: String,
    pub title: String,
    pub entries: Vec<ManifestEntry>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ManifestEntry {
    pub id: String,
    pub title: String,
    pub pack_id: String,
    pub version: String,
    pub language: Language,
    pub taxonomy: Vec<Taxonomy>,
    pub tags: Vec<String>,
    pub path: String,
    pub abstract_text: String,
    #[serde(default)]
    pub trust: TrustState,
    #[serde(default)]
    pub staleness: Staleness,
    #[serde(default)]
    pub token_budget: TokenBudget,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct BundledPack {
    pub schema_version: u16,
    pub pack_id: String,
    pub title: String,
    pub books: Vec<Book>,
}

const BUNDLED_PACKS: &[(&str, &str)] = &[
    (
        "foundations",
        include_str!("../../../bundled/books/foundations.json"),
    ),
    ("systems", include_str!("../../../bundled/books/systems.json")),
    ("data", include_str!("../../../bundled/books/data.json")),
    ("security", include_str!("../../../bundled/books/security.json")),
    ("product", include_str!("../../../bundled/books/product.json")),
    ("design", include_str!("../../../bundled/books/design.json")),
    (
        "management",
        include_str!("../../../bundled/books/management.json"),
    ),
    ("research", include_str!("../../../bundled/books/research.json")),
    ("writing", include_str!("../../../bundled/books/writing.json")),
    (
        "operations",
        include_str!("../../../bundled/books/operations.json"),
    ),
];

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SignedRegistryManifest {
    pub schema_version: u16,
    pub registry_id: String,
    pub key_id: String,
    pub content_digest: String,
    pub signature: String,
    pub packs: Vec<ManifestPack>,
}

impl SignedRegistryManifest {
    pub fn bundled() -> Result<Self, serde_json::Error> {
        serde_json::from_str(BUNDLED_MANIFEST)
    }

    pub fn validate(&self) -> Result<(), ManifestError> {
        if self.schema_version != BOOK_SCHEMA_VERSION {
            return Err(ManifestError::SchemaVersion(self.schema_version));
        }
        if self.key_id.is_empty() || self.signature.is_empty() || self.content_digest.is_empty() {
            return Err(ManifestError::MissingSignature);
        }
        if !self.key_id.starts_with("builtin:") || !self.signature.starts_with("builtin:") {
            return Err(ManifestError::UnsupportedSignatureScheme);
        }
        if self.registry_id.trim().is_empty() || self.packs.is_empty() {
            return Err(ManifestError::InvalidRegistry);
        }
        let mut ids = std::collections::BTreeSet::new();
        let mut pack_ids = std::collections::BTreeSet::new();
        for pack in &self.packs {
            if pack.id.trim().is_empty() || pack.title.trim().is_empty() || !pack_ids.insert(pack.id.clone())
            {
                return Err(ManifestError::InvalidPack(pack.id.clone()));
            }
            if pack.entries.is_empty() {
                return Err(ManifestError::EmptyPack(pack.id.clone()));
            }
            for entry in &pack.entries {
                if entry.pack_id != pack.id
                    || entry.id.trim().is_empty()
                    || entry.title.trim().is_empty()
                    || !ids.insert(entry.id.clone())
                    || entry.path.is_empty()
                    || entry.abstract_text.trim().is_empty()
                    || !entry.token_budget.valid()
                {
                    return Err(ManifestError::InvalidEntry(entry.id.clone()));
                }
            }
        }
        Ok(())
    }

    pub fn entry_count(&self) -> usize {
        self.packs.iter().map(|pack| pack.entries.len()).sum()
    }
}

/// Hashes the exact embedded pack bytes in deterministic registry order.
/// This is an integrity check for compile-trusted content, not a signature.
pub fn bundled_content_digest() -> String {
    let mut hasher = Sha256::new();
    for (pack_id, bytes) in BUNDLED_PACKS {
        hasher.update(pack_id.as_bytes());
        hasher.update([0]);
        hasher.update(bytes.as_bytes());
    }
    format!("sha256:{:x}", hasher.finalize())
}

pub fn bundled_packs() -> Result<Vec<BundledPack>, BundledBooksError> {
    BUNDLED_PACKS
        .iter()
        .map(|(_, source)| serde_json::from_str(source).map_err(BundledBooksError::Json))
        .collect()
}

pub fn bundled_books() -> Result<Vec<Book>, BundledBooksError> {
    validate_bundled_registry().map(|packs| packs.into_iter().flat_map(|pack| pack.books).collect())
}

pub fn validate_bundled_registry() -> Result<Vec<BundledPack>, BundledBooksError> {
    let manifest = SignedRegistryManifest::bundled()?;
    manifest.validate().map_err(BundledBooksError::Manifest)?;
    if manifest.key_id != BUILTIN_BOOK_KEY_ID || manifest.signature != BUILTIN_BOOK_SIGNATURE {
        return Err(BundledBooksError::Integrity(
            "bundled registry must use the explicit builtin provenance scheme".into(),
        ));
    }
    if manifest.content_digest != bundled_content_digest() {
        return Err(BundledBooksError::Integrity(
            "bundled pack bytes do not match manifest content_digest".into(),
        ));
    }
    if manifest.packs.len() != BUNDLED_PACK_COUNT || manifest.entry_count() < MIN_BUNDLED_ENTRY_COUNT {
        return Err(BundledBooksError::Integrity(
            "bundled catalog does not meet its pack or entry floor".into(),
        ));
    }
    let packs = bundled_packs()?;
    if packs.len() != manifest.packs.len() {
        return Err(BundledBooksError::Integrity(
            "manifest pack count does not match bundled pack files".into(),
        ));
    }
    let mut books = std::collections::BTreeMap::new();
    for (pack, (path_id, _)) in packs.iter().zip(BUNDLED_PACKS) {
        if pack.schema_version != BOOK_SCHEMA_VERSION || pack.pack_id != *path_id {
            return Err(BundledBooksError::Integrity(format!(
                "invalid bundled pack {}",
                pack.pack_id
            )));
        }
        let manifest_pack = manifest
            .packs
            .iter()
            .find(|candidate| candidate.id == pack.pack_id)
            .ok_or_else(|| {
                BundledBooksError::Integrity(format!("pack {} is not in the manifest", pack.pack_id))
            })?;
        if manifest_pack.entries.len() != pack.books.len() {
            return Err(BundledBooksError::Integrity(format!(
                "pack {} entry count mismatch",
                pack.pack_id
            )));
        }
        for book in &pack.books {
            let report = verify_draft(book);
            if !report.valid()
                || book.metadata.trust == TrustState::Draft
                || book.metadata.trust == TrustState::Unverified
            {
                return Err(BundledBooksError::Integrity(format!(
                    "book {} failed content verification",
                    book.metadata.id
                )));
            }
            if book.metadata.staleness.status != Freshness::Current {
                return Err(BundledBooksError::Integrity(format!(
                    "book {} is not current",
                    book.metadata.id
                )));
            }
            let entry = manifest_pack
                .entries
                .iter()
                .find(|candidate| candidate.id == book.metadata.id)
                .ok_or_else(|| {
                    BundledBooksError::Integrity(format!("book {} is not in the manifest", book.metadata.id))
                })?;
            let expected_path = format!("bundled/books/{}.json", pack.pack_id);
            if entry.path != expected_path
                || entry.path != book.metadata.path
                || entry.version != book.metadata.version
                || entry.abstract_text != book.metadata.abstract_text
                || entry.trust != book.metadata.trust
                || entry.staleness != book.metadata.staleness
                || entry.token_budget != book.metadata.token_budget
            {
                return Err(BundledBooksError::Integrity(format!(
                    "manifest metadata mismatch for {}",
                    book.metadata.id
                )));
            }
            if books.insert(book.metadata.id.clone(), book).is_some() {
                return Err(BundledBooksError::Integrity(format!(
                    "duplicate bundled book {}",
                    book.metadata.id
                )));
            }
        }
    }
    if books.len() != manifest.entry_count() {
        return Err(BundledBooksError::Integrity(
            "manifest and bundled book IDs differ".into(),
        ));
    }
    Ok(packs)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ManifestError {
    SchemaVersion(u16),
    MissingSignature,
    EmptyPack(String),
    InvalidEntry(String),
    InvalidPack(String),
    InvalidRegistry,
    UnsupportedSignatureScheme,
}

#[derive(Debug, Error)]
pub enum BundledBooksError {
    #[error("bundled JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("manifest: {0:?}")]
    Manifest(ManifestError),
    #[error("bundled integrity: {0}")]
    Integrity(String),
}

impl Book {
    /// Returns the bounded text used by local lexical retrieval.
    pub fn compact_retrieval(&self) -> String {
        let mut parts = vec![self.metadata.title.clone(), self.metadata.abstract_text.clone()];
        for chapter in &self.chapters {
            parts.push(chapter.title.clone());
            parts.push(chapter.summary.clone());
            for section in &chapter.sections {
                parts.push(section.title.clone());
                parts.push(section.summary.clone());
            }
        }
        parts.extend(self.key_facts.iter().map(|fact| fact.statement.clone()));
        parts.push(self.metadata.source.title.clone());
        parts.join(" ")
    }

    pub fn compact_text(&self) -> String {
        self.compact_retrieval()
    }

    pub fn compact_token_count(&self) -> u32 {
        terms(&self.compact_text()).len() as u32
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CatalogError {
    DuplicateId(String),
    InvalidDraft(Vec<String>),
    NotPromotable(String),
    MissingBook(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerificationReport {
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

impl VerificationReport {
    pub fn valid(&self) -> bool {
        self.errors.is_empty()
    }
}

pub fn verify_draft(book: &Book) -> VerificationReport {
    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    let m = &book.metadata;
    if m.schema_version != BOOK_SCHEMA_VERSION {
        errors.push("unsupported schema version".into());
    }
    if m.id.trim().is_empty() || m.title.trim().is_empty() || m.version.trim().is_empty() {
        errors.push("id, title, and version are required".into());
    }
    if m.path.trim().is_empty() || m.abstract_text.trim().is_empty() {
        errors.push("path and abstract are required".into());
    }
    if m.authors.is_empty() || m.taxonomy.is_empty() || m.tags.is_empty() {
        errors.push("author, taxonomy, and tags are required".into());
    }
    if !m.token_budget.valid() {
        errors.push("token budget must be positive and ordered".into());
    }
    if m.source.title.trim().is_empty()
        || m.source.publisher.trim().is_empty()
        || m.source.note.trim().is_empty()
    {
        errors.push("source title, publisher, and note are required".into());
    }
    if m.trust == TrustState::Stale && m.staleness.status != Freshness::Stale {
        errors.push("stale trust requires stale freshness".into());
    }
    if m.trust == TrustState::Stale {
        errors.push("stale books require refresh before verification".into());
    }
    if m.trust != TrustState::Stale && m.staleness.status == Freshness::Stale {
        errors.push("stale freshness requires stale trust".into());
    }
    if m.staleness.status == Freshness::Current
        && (m.staleness.checked_at.as_deref().is_none_or(str::is_empty)
            || m.staleness.review_after.as_deref().is_none_or(str::is_empty))
    {
        errors.push("current freshness requires checked_at and review_after".into());
    }
    if m.staleness.status == Freshness::Stale && m.staleness.reason.trim().is_empty() {
        errors.push("stale freshness requires a reason".into());
    }
    if book.chapters.is_empty() {
        errors.push("at least one chapter is required".into());
    }
    if book.key_facts.is_empty() {
        warnings.push("book has no key facts".into());
    }
    if book.citations.is_empty() {
        errors.push("at least one citation is required".into());
    }
    let mut chapter_ids = std::collections::BTreeSet::new();
    let mut section_ids = std::collections::BTreeSet::new();
    let mut section_fact_ids = Vec::new();
    for chapter in &book.chapters {
        if chapter.id.trim().is_empty()
            || chapter.title.trim().is_empty()
            || chapter.summary.trim().is_empty()
        {
            errors.push("chapter id, title, and summary are required".into());
        }
        if !chapter_ids.insert(chapter.id.as_str()) {
            errors.push(format!("duplicate chapter {}", chapter.id));
        }
        if chapter.sections.is_empty() {
            errors.push(format!("chapter {} has no sections", chapter.id));
        }
        for section in &chapter.sections {
            if section.id.trim().is_empty()
                || section.title.trim().is_empty()
                || section.summary.trim().is_empty()
            {
                errors.push("section id, title, and summary are required".into());
            }
            if !section_ids.insert(section.id.as_str()) {
                errors.push(format!("duplicate section {}", section.id));
            }
            if section.key_facts.is_empty() {
                warnings.push(format!("section {} has no key facts", section.id));
            }
            section_fact_ids.extend(section.key_facts.iter().map(String::as_str));
        }
    }
    let mut fact_ids = std::collections::BTreeSet::new();
    let citation_ids: std::collections::BTreeSet<_> = book.citations.iter().map(|c| c.id.as_str()).collect();
    if citation_ids.len() != book.citations.len() {
        errors.push("citation IDs must be unique".into());
    }
    for citation in &book.citations {
        if citation.locator.trim().is_empty()
            || citation.source.trim().is_empty()
            || citation.note.trim().is_empty()
        {
            errors.push(format!("citation {} is incomplete", citation.id));
        }
        if citation.kind != SourceKind::CuratedEditorial && citation.url.as_deref().is_none_or(str::is_empty)
        {
            errors.push(format!("citation {} needs a source URL", citation.id));
        }
    }
    for fact in &book.key_facts {
        if fact.id.trim().is_empty() || fact.statement.trim().is_empty() || !fact_ids.insert(fact.id.as_str())
        {
            errors.push(format!("invalid or duplicate fact {}", fact.id));
        }
        for citation in &fact.citation_ids {
            if !citation_ids.contains(citation.as_str()) {
                errors.push(format!("fact {} cites missing {}", fact.id, citation));
            }
        }
    }
    for fact_id in section_fact_ids {
        if !fact_ids.contains(fact_id) {
            errors.push(format!("section cites missing fact {}", fact_id));
        }
    }
    VerificationReport { errors, warnings }
}

#[derive(Clone, Debug, Default)]
pub struct Catalog {
    books: BTreeMap<String, Book>,
    revisions: BTreeMap<String, String>,
}

impl Catalog {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn len(&self) -> usize {
        self.books.len()
    }
    pub fn is_empty(&self) -> bool {
        self.books.is_empty()
    }
    pub fn get(&self, id: &str) -> Option<&Book> {
        self.books.get(id)
    }
    pub fn iter(&self) -> impl Iterator<Item = &Book> {
        self.books.values()
    }

    pub fn insert_draft(&mut self, mut book: Book) -> Result<VerificationReport, CatalogError> {
        if self.books.contains_key(&book.metadata.id) {
            return Err(CatalogError::DuplicateId(book.metadata.id));
        }
        book.metadata.trust = TrustState::Draft;
        let report = verify_draft(&book);
        if !report.valid() {
            return Err(CatalogError::InvalidDraft(report.errors));
        }
        self.revisions
            .insert(book.metadata.id.clone(), book.metadata.version.clone());
        self.books.insert(book.metadata.id.clone(), book);
        Ok(report)
    }

    pub fn verify(&mut self, id: &str) -> Result<VerificationReport, CatalogError> {
        let book = self
            .books
            .get_mut(id)
            .ok_or_else(|| CatalogError::MissingBook(id.into()))?;
        let report = verify_draft(book);
        if report.valid() {
            book.metadata.trust = TrustState::Verified;
        }
        Ok(report)
    }

    pub fn promote(&mut self, id: &str) -> Result<(), CatalogError> {
        let book = self
            .books
            .get_mut(id)
            .ok_or_else(|| CatalogError::MissingBook(id.into()))?;
        if book.metadata.trust != TrustState::Verified || book.metadata.staleness.status != Freshness::Current
        {
            return Err(CatalogError::NotPromotable(id.into()));
        }
        if !verify_draft(book).valid() {
            return Err(CatalogError::NotPromotable(id.into()));
        }
        book.metadata.trust = TrustState::Promoted;
        Ok(())
    }

    pub fn demote_stale(&mut self, id: &str, current_version: &str) -> Result<(), CatalogError> {
        let book = self
            .books
            .get_mut(id)
            .ok_or_else(|| CatalogError::MissingBook(id.into()))?;
        if book.metadata.version != current_version {
            book.metadata.trust = TrustState::Stale;
            book.metadata.staleness.status = Freshness::Stale;
            book.metadata.staleness.reason = format!(
                "Indexed version {} differs from current version {}.",
                book.metadata.version, current_version
            );
        }
        Ok(())
    }

    pub fn search(
        &self,
        query: &str,
        language: Option<Language>,
        tags: &[&str],
        limit: usize,
    ) -> Vec<SearchHit<'_>> {
        let query_terms = terms(query);
        let mut hits: Vec<_> = self
            .books
            .values()
            .filter(|book| book.metadata.trust.searchable())
            .map(|book| {
                let m = &book.metadata;
                let haystack = terms(&format!(
                    "{} {} {} {} {}",
                    m.title,
                    m.abstract_text,
                    m.path,
                    m.tags.join(" "),
                    book.compact_text()
                ));
                let lexical = query_terms
                    .iter()
                    .filter(|term| haystack.iter().any(|word| word == *term))
                    .count() as i32;
                let path = if query_terms
                    .iter()
                    .any(|term| m.path.to_ascii_lowercase().contains(term))
                {
                    3
                } else {
                    0
                };
                let language_score = language.is_some_and(|want| want == m.language) as i32 * 4;
                let tag_score = tags
                    .iter()
                    .filter(|tag| m.tags.iter().any(|candidate| candidate.eq_ignore_ascii_case(tag)))
                    .count() as i32
                    * 5;
                SearchHit {
                    book,
                    score: lexical * 10 + path + language_score + tag_score + i32::from(m.trust.rank()),
                }
            })
            .filter(|hit| hit.score > 0)
            .collect();
        hits.sort_by(|a, b| {
            b.score
                .cmp(&a.score)
                .then_with(|| a.book.metadata.path.cmp(&b.book.metadata.path))
                .then_with(|| a.book.metadata.id.cmp(&b.book.metadata.id))
        });
        hits.truncate(limit);
        hits
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SearchHit<'a> {
    pub book: &'a Book,
    pub score: i32,
}

fn terms(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(|word| word.to_ascii_lowercase())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn book(id: &str, trust: TrustState) -> Book {
        Book {
            metadata: BookMetadata {
                schema_version: BOOK_SCHEMA_VERSION,
                id: id.into(),
                version: "1.0.0".into(),
                title: format!("{id} systems guide"),
                authors: vec!["Minha Editorial".into()],
                language: Language::English,
                taxonomy: vec![Taxonomy::Systems],
                tags: vec!["systems".into(), "rust".into()],
                path: format!("bundled/books/{id}.json"),
                abstract_text: "A practical guide to systems and reliable software.".into(),
                trust,
                staleness: Staleness::current(),
                token_budget: TokenBudget::default(),
                source: SourceMetadata {
                    kind: SourceKind::CuratedEditorial,
                    title: "Minha test editorial synthesis".into(),
                    publisher: "Minha project maintainers".into(),
                    url: None,
                    accessed: Some("2026-07-29".into()),
                    license: Some("Apache-2.0".into()),
                    note: "Test synthesis; not a quotation.".into(),
                },
            },
            chapters: vec![Chapter {
                id: "ch-1".into(),
                title: "Foundations".into(),
                summary: "The foundation.".into(),
                sections: vec![Section {
                    id: "section-1".into(),
                    title: "Boundary".into(),
                    summary: "The boundary is explicit.".into(),
                    key_facts: vec!["fact-1".into()],
                }],
            }],
            key_facts: vec![KeyFact {
                id: "fact-1".into(),
                statement: "Small explicit interfaces are easier to verify.".into(),
                tags: vec!["systems".into()],
                citation_ids: vec!["cite-1".into()],
            }],
            citations: vec![Citation {
                id: "cite-1".into(),
                locator: "chapter-1".into(),
                source: "first-party notes".into(),
                note: "Editorial seed.".into(),
                kind: SourceKind::CuratedEditorial,
                url: None,
                accessed: Some("2026-07-29".into()),
            }],
        }
    }

    #[test]
    fn bundled_registry_has_ten_packs_one_hundred_entries_and_resolved_content() {
        let manifest = SignedRegistryManifest::bundled().expect("test operation should succeed");
        manifest.validate().expect("test operation should succeed");
        assert_eq!(manifest.key_id, BUILTIN_BOOK_KEY_ID);
        assert_eq!(manifest.signature, BUILTIN_BOOK_SIGNATURE);
        assert_eq!(manifest.content_digest, bundled_content_digest());
        assert_eq!(manifest.content_digest.len(), "sha256:".len() + 64);
        assert_eq!(manifest.packs.len(), 10);
        assert_eq!(manifest.entry_count(), 100);
        let packs = validate_bundled_registry().expect("test operation should succeed");
        assert_eq!(packs.len(), 10);
        assert!(packs.iter().all(|pack| pack.books.len() == 10));
        assert_eq!(bundled_books().expect("test operation should succeed").len(), 100);
    }

    #[test]
    fn bundled_schema_ids_references_and_budgets_are_consistent() {
        let manifest = SignedRegistryManifest::bundled().expect("test operation should succeed");
        let paths: std::collections::BTreeSet<_> = BUNDLED_PACKS
            .iter()
            .map(|(id, _)| format!("bundled/books/{id}.json"))
            .collect();
        let mut ids = std::collections::BTreeSet::new();
        assert_eq!(manifest.schema_version, BOOK_SCHEMA_VERSION);
        for pack in &manifest.packs {
            for entry in &pack.entries {
                assert!(paths.contains(&entry.path));
                assert!(ids.insert(entry.id.clone()));
                assert!(entry.token_budget.valid());
                assert_eq!(entry.staleness.status, Freshness::Current);
                assert_eq!(entry.trust, TrustState::Promoted);
            }
        }
        assert_eq!(ids.len(), 100);
    }

    #[test]
    fn manifest_rejects_schema_drift_and_duplicate_ids() {
        let mut wrong_schema = SignedRegistryManifest::bundled().expect("test operation should succeed");
        wrong_schema.schema_version = BOOK_SCHEMA_VERSION + 1;
        assert_eq!(
            wrong_schema.validate(),
            Err(ManifestError::SchemaVersion(BOOK_SCHEMA_VERSION + 1))
        );

        let mut duplicate = SignedRegistryManifest::bundled().expect("test operation should succeed");
        duplicate.packs[1].entries[0].id = duplicate.packs[0].entries[0].id.clone();
        assert_eq!(
            duplicate.validate(),
            Err(ManifestError::InvalidEntry(
                duplicate.packs[1].entries[0].id.clone()
            ))
        );

        let mut external = SignedRegistryManifest::bundled().expect("test operation should succeed");
        external.signature = "external:unverified".into();
        assert_eq!(
            external.validate(),
            Err(ManifestError::UnsupportedSignatureScheme)
        );
    }

    #[test]
    fn bundled_content_has_detailed_sections_sources_and_compact_budgets() {
        for book in bundled_books().expect("test operation should succeed") {
            assert_eq!(book.metadata.schema_version, BOOK_SCHEMA_VERSION);
            assert!(book.chapters.iter().all(|chapter| chapter.sections.len() >= 2));
            assert!(book.key_facts.len() >= 3);
            assert!(book.citations.len() >= 2);
            assert!(book.compact_token_count() <= book.metadata.token_budget.detailed_tokens);
            assert!(!book.metadata.source.note.contains('"'));
        }
    }

    #[test]
    fn legacy_json_deserializes_with_defaulted_evolved_fields() {
        let value = serde_json::json!({
            "schema_version": 1,
            "id": "legacy",
            "version": "1.0.0",
            "title": "Legacy",
            "authors": ["Author"],
            "language": "english",
            "taxonomy": ["systems"],
            "tags": ["legacy"],
            "path": "legacy.json",
            "abstract_text": "Legacy abstract",
            "trust": "unverified"
        });
        let metadata: BookMetadata = serde_json::from_value(value).expect("test operation should succeed");
        assert_eq!(metadata.staleness.status, Freshness::Unknown);
        assert_eq!(metadata.token_budget, TokenBudget::default());
        assert_eq!(metadata.source.kind, SourceKind::CuratedEditorial);
    }

    #[test]
    fn lifecycle_requires_verification_before_promotion() {
        let mut catalog = Catalog::new();
        catalog
            .insert_draft(book("systems", TrustState::Unverified))
            .expect("test operation should succeed");
        assert!(catalog.promote("systems").is_err());
        catalog.verify("systems").expect("test operation should succeed");
        catalog.promote("systems").expect("test operation should succeed");
        assert_eq!(
            catalog
                .get("systems")
                .expect("test operation should succeed")
                .metadata
                .trust,
            TrustState::Promoted
        );
    }

    #[test]
    fn retrieval_is_deterministic_and_stale_is_demoted() {
        let mut catalog = Catalog::new();
        catalog
            .insert_draft(book("zeta", TrustState::Draft))
            .expect("test operation should succeed");
        catalog
            .insert_draft(book("alpha", TrustState::Draft))
            .expect("test operation should succeed");
        catalog.verify("zeta").expect("test operation should succeed");
        catalog.verify("alpha").expect("test operation should succeed");
        let first: Vec<_> = catalog
            .search("systems", None, &["rust"], 10)
            .iter()
            .map(|hit| hit.book.metadata.id.as_str())
            .collect();
        let second: Vec<_> = catalog
            .search("systems", None, &["rust"], 10)
            .iter()
            .map(|hit| hit.book.metadata.id.as_str())
            .collect();
        assert_eq!(first, second);
        catalog
            .demote_stale("alpha", "2.0.0")
            .expect("test operation should succeed");
        assert_eq!(
            catalog
                .get("alpha")
                .expect("test operation should succeed")
                .metadata
                .trust,
            TrustState::Stale
        );
        assert_eq!(
            catalog
                .get("alpha")
                .expect("test operation should succeed")
                .metadata
                .staleness
                .status,
            Freshness::Stale
        );
        assert!(
            !catalog
                .verify("alpha")
                .expect("test operation should succeed")
                .valid()
        );
        assert_eq!(
            catalog
                .get("alpha")
                .expect("test operation should succeed")
                .metadata
                .trust,
            TrustState::Stale
        );
        assert!(catalog.promote("alpha").is_err());
    }

    #[test]
    fn draft_books_are_hidden_but_verified_content_is_compactly_searchable() {
        let mut catalog = Catalog::new();
        catalog
            .insert_draft(book("draft", TrustState::Unverified))
            .expect("test operation should succeed");
        assert!(catalog.search("explicit interfaces", None, &[], 10).is_empty());
        catalog.verify("draft").expect("test operation should succeed");
        catalog.promote("draft").expect("test operation should succeed");
        let hits = catalog.search("explicit interfaces", None, &[], 10);
        assert_eq!(hits.len(), 1);
        assert!(
            hits[0]
                .book
                .compact_retrieval()
                .contains("Small explicit interfaces")
        );
    }

    #[test]
    fn budgets_are_model_aware_and_versioned() {
        assert_eq!(RetrievalBudget::for_model("spark").input_tokens, 16_000);
        assert_eq!(RetrievalBudget::for_model("luna").input_tokens, 32_000);
        assert_eq!(RetrievalBudget::for_model("unknown").input_tokens, 4_000);
        for book in bundled_books().expect("test operation should succeed") {
            assert!(book.metadata.token_budget.valid());
            assert!(book.compact_token_count() <= book.metadata.token_budget.detailed_tokens);
        }
    }
}
