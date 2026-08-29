//! Guards `.config/wt.toml` against TOML's bare-key-binds-to-last-table rule.
//!
//! Not a scenario — nothing here drives the CLI. It is a lint over the repo's
//! own worktrunk config, which is where this project dogfoods itself: the
//! `pre-merge` hook is the `cargo affected run` gate that must pass before a
//! branch merges.
//!
//! TOML binds a bare key to the most recent table header, so writing
//! `pre-merge = "…"` *below* the `[[post-merge]]` blocks silently files it
//! inside the last `[[post-merge]]` entry. Worktrunk then deserializes
//! `pre_merge` as `None` and runs the command as a post-merge hook instead —
//! the gate never gates, and nothing reports an error, because both spellings
//! are valid TOML and valid worktrunk config. That is exactly the shape of
//! failure this crate exists to avoid, so it gets a tripwire.

use toml_edit::{DocumentMut, Item};

/// Every hook key worktrunk reads at the root of a project config
/// (`HooksConfig` in `worktrunk/src/config/hooks.rs`). A hook name showing up
/// *inside* a `[[post-merge]]` entry means it was swallowed by the array.
const HOOK_KEYS: &[&str] = &[
    "pre-switch",
    "post-switch",
    "pre-start",
    "pre-create",
    "post-start",
    "post-create",
    "pre-commit",
    "post-commit",
    "pre-merge",
    "post-merge",
    "pre-remove",
    "post-remove",
];

fn wt_config() -> DocumentMut {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/.config/wt.toml");
    std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("failed to read {path}: {e}"))
        .parse()
        .unwrap_or_else(|e| panic!("{path} is not valid TOML: {e}"))
}

#[test]
fn pre_merge_hook_is_at_the_document_root() {
    let doc = wt_config();
    assert!(
        doc.get("pre-merge").is_some(),
        "`pre-merge` must be a root key of .config/wt.toml — worktrunk reads \
         hooks off the root table. Keys written after a `[[post-merge]]` \
         header belong to that entry instead. Current root keys: {:?}",
        doc.iter().map(|(k, _)| k).collect::<Vec<_>>(),
    );
}

#[test]
fn no_hook_key_is_nested_inside_a_hook_array() {
    let doc = wt_config();
    for (name, item) in doc.iter() {
        let Item::ArrayOfTables(entries) = item else {
            continue;
        };
        for (i, entry) in entries.iter().enumerate() {
            for nested in entry.iter().map(|(k, _)| k) {
                assert!(
                    !HOOK_KEYS.contains(&nested),
                    "hook key `{nested}` is nested inside `[[{name}]]` entry \
                     {i} — it was almost certainly meant as a root key and \
                     got bound to the preceding table header. Move it above \
                     the first `[[{name}]]` block.",
                );
            }
        }
    }
}
