//! Template expansion and skill file installation for `repm init`.
//!
//! The CLAUDE.md template uses `{{variable}}` placeholders that are
//! filled from `repromagic_config.toml` at init time.
//!
//! Skill files are static content embedded in the binary — no
//! template expansion needed.

use std::path::Path;

use anyhow::{Context, Result};

use crate::config::RepromagicConfig;

// ---------------------------------------------------------------------------
// Embedded templates
// ---------------------------------------------------------------------------

/// CLAUDE.md template with `{{variable}}` placeholders.
const CLAUDE_MD_TEMPLATE: &str = include_str!("../templates/CLAUDE.md.tmpl");

/// Skill files — static, no templating needed.
/// Each tuple is (relative_path, content).
const SKILL_FILES: &[(&str, &str)] = &[
    (
        "vng-usage/SKILL.md",
        include_str!("../templates/skills/vng-usage/SKILL.md"),
    ),
    (
        "scxsim-usage/SKILL.md",
        include_str!("../templates/skills/scxsim-usage/SKILL.md"),
    ),
    (
        "rtapp-usage/SKILL.md",
        include_str!("../templates/skills/rtapp-usage/SKILL.md"),
    ),
    (
        "experiment-mgmt/SKILL.md",
        include_str!("../templates/skills/experiment-mgmt/SKILL.md"),
    ),
    (
        "capture/SKILL.md",
        include_str!("../templates/skills/capture/SKILL.md"),
    ),
];

// ---------------------------------------------------------------------------
// Template expansion
// ---------------------------------------------------------------------------

/// Expand `{{variable}}` placeholders in the CLAUDE.md template using
/// values from the workspace config.
fn expand_claude_md(config: &RepromagicConfig) -> String {
    let mut output = CLAUDE_MD_TEMPLATE.to_string();

    // Project fields
    output = output.replace("{{project.name}}", &config.project.name);
    output = output.replace("{{project.phenomenon}}", &config.project.phenomenon.to_string());
    output = output.replace(
        "{{project.description}}",
        if config.project.description.is_empty() {
            "(no description provided)"
        } else {
            &config.project.description
        },
    );

    // Defaults
    output = output.replace("{{defaults.cores}}", &config.defaults.cores.to_string());
    output = output.replace("{{defaults.duration}}", &config.defaults.duration.to_string());
    output = output.replace("{{defaults.reps}}", &config.defaults.reps.to_string());

    output
}

// ---------------------------------------------------------------------------
// Installation
// ---------------------------------------------------------------------------

/// Install CLAUDE.md and skill files into a workspace root.
///
/// Called by `repm init` after creating the workspace directory structure.
/// Skips files that already exist (allows user customization).
pub fn install_templates(workspace_root: &Path, config: &RepromagicConfig) -> Result<()> {
    // 1. Write CLAUDE.md
    let claude_path = workspace_root.join("CLAUDE.md");
    if claude_path.exists() {
        eprintln!("  CLAUDE.md already exists — skipping");
    } else {
        let content = expand_claude_md(config);
        std::fs::write(&claude_path, content)
            .with_context(|| format!("Failed to write {}", claude_path.display()))?;
        eprintln!("  wrote CLAUDE.md");
    }

    // 2. Install skill files
    let skills_dir = workspace_root.join(".claude").join("skills");
    let mut installed = 0;
    let mut skipped = 0;

    for (rel_path, content) in SKILL_FILES {
        let dest = skills_dir.join(rel_path);

        if dest.exists() {
            skipped += 1;
            continue;
        }

        // Ensure parent directory exists
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create {}", parent.display()))?;
        }

        std::fs::write(&dest, content)
            .with_context(|| format!("Failed to write {}", dest.display()))?;
        installed += 1;
    }

    if installed > 0 {
        eprintln!("  installed {} skill files to .claude/skills/", installed);
    }
    if skipped > 0 {
        eprintln!("  skipped {} existing skill files", skipped);
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Defaults, Phenomenon, ProjectInfo, RepromagicConfig};
    use std::collections::BTreeMap;

    fn test_config() -> RepromagicConfig {
        RepromagicConfig {
            project: ProjectInfo {
                name: "cache_svc".to_string(),
                phenomenon: Phenomenon::BadTailLatency,
                description: "Evaluate scheduler impact on cache serving".to_string(),
            },
            defaults: Defaults {
                cores: 12,
                duration: 30,
                reps: 5,
                warmup: 5,
            },
            topology: Default::default(),
            schedulers: BTreeMap::new(),
            workload: Default::default(),
            capture: Default::default(),
        }
    }

    #[test]
    fn test_expand_project_name() {
        let config = test_config();
        let output = expand_claude_md(&config);
        assert!(output.contains("# cache_svc: Experiment Workspace"));
        assert!(output.contains("**cache_svc**"));
    }

    #[test]
    fn test_expand_phenomenon() {
        let config = test_config();
        let output = expand_claude_md(&config);
        assert!(output.contains("**Phenomenon:** bad_tail_latency"));
    }

    #[test]
    fn test_expand_description() {
        let config = test_config();
        let output = expand_claude_md(&config);
        assert!(output.contains("Evaluate scheduler impact on cache serving"));
    }

    #[test]
    fn test_expand_empty_description() {
        let mut config = test_config();
        config.project.description = String::new();
        let output = expand_claude_md(&config);
        assert!(output.contains("(no description provided)"));
    }

    #[test]
    fn test_expand_defaults() {
        let config = test_config();
        let output = expand_claude_md(&config);
        assert!(output.contains("repm run --reps 5"));
        // Branch name uses project name
        assert!(output.contains("repro/cache_svc"));
    }

    #[test]
    fn test_no_unexpanded_placeholders() {
        let config = test_config();
        let output = expand_claude_md(&config);
        // No remaining {{...}} placeholders
        assert!(
            !output.contains("{{"),
            "Found unexpanded placeholder in output:\n{}",
            output
                .lines()
                .filter(|l| l.contains("{{"))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }

    #[test]
    fn test_skill_files_embedded() {
        assert_eq!(SKILL_FILES.len(), 5);
        for (path, content) in SKILL_FILES {
            assert!(!content.is_empty(), "Skill file {} is empty", path);
            assert!(
                content.starts_with("---"),
                "Skill file {} missing YAML frontmatter",
                path
            );
        }
    }

    #[test]
    fn test_install_creates_files() {
        let tmp = std::env::temp_dir().join("repm_test_install_templates");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let config = test_config();
        install_templates(&tmp, &config).unwrap();

        // CLAUDE.md exists
        assert!(tmp.join("CLAUDE.md").exists());
        let claude_content = std::fs::read_to_string(tmp.join("CLAUDE.md")).unwrap();
        assert!(claude_content.contains("cache_svc"));

        // Skill files exist
        assert!(tmp.join(".claude/skills/vng-usage/SKILL.md").exists());
        assert!(tmp.join(".claude/skills/scxsim-usage/SKILL.md").exists());
        assert!(tmp.join(".claude/skills/rtapp-usage/SKILL.md").exists());
        assert!(tmp.join(".claude/skills/experiment-mgmt/SKILL.md").exists());
        assert!(tmp.join(".claude/skills/capture/SKILL.md").exists());

        // Clean up
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_install_skips_existing() {
        let tmp = std::env::temp_dir().join("repm_test_install_skip");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        // Pre-create CLAUDE.md with custom content
        let custom = "# My custom CLAUDE.md\n";
        std::fs::write(tmp.join("CLAUDE.md"), custom).unwrap();

        let config = test_config();
        install_templates(&tmp, &config).unwrap();

        // CLAUDE.md should NOT be overwritten
        let content = std::fs::read_to_string(tmp.join("CLAUDE.md")).unwrap();
        assert_eq!(content, custom);

        // But skill files should be installed
        assert!(tmp.join(".claude/skills/vng-usage/SKILL.md").exists());

        // Clean up
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
