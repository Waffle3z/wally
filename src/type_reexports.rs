use anyhow::{bail, Context, Result};
use full_moon::ast::LastStmt;
use log::{error, info, warn};
use std::path::{Path, PathBuf};

use crate::require_parser::*;

// Filesystem-based resolver
fn resolve_components_fs(
    link_path: &Path,
    packages_root: &Path,
    path_components: &[String],
) -> Option<PathBuf> {
    if path_components.is_empty() {
        return None;
    }

    let first = &path_components[0];
    let mut cursor: PathBuf;

    if first == "script" {
        // Start from the link file path (`script`), allow Parent to walk up to the link's directory
        cursor = link_path.to_path_buf();
        for comp in path_components.iter().skip(1) {
            if comp == "Parent" {
                cursor = cursor.parent()?.to_path_buf();
            } else {
                cursor.push(comp);
            }
        }
    } else if first == "game" {
        // Map game.<...>.Packages.* to the on-disk Packages root for this pass
        cursor = packages_root.to_path_buf();

        let mut started = false;
        for comp in path_components.iter().skip(1) {
            if !started {
                // Skip components until we hit "Packages"
                if comp == "Packages" {
                    started = true;
                }
                continue;
            }

            if comp == "Parent" {
                cursor = cursor.parent()?.to_path_buf();
            } else {
                cursor.push(comp);
            }
        }

        // Fallback: if "Packages" wasn't found, try pushing everything after 'game'
        if !started {
            for comp in path_components.iter().skip(1) {
                if comp == "Parent" {
                    cursor = cursor.parent()?.to_path_buf();
                } else {
                    cursor.push(comp);
                }
            }
        }
    } else {
        return None;
    }

    // If direct file exists, use it.
    if cursor.is_file() {
        return Some(cursor);
    }

    // Try {name}.lua / {name}.luau
    let candidate_lua = cursor.with_extension("lua");
    if candidate_lua.is_file() {
        return Some(candidate_lua);
    }
    let candidate_luau = cursor.with_extension("luau");
    if candidate_luau.is_file() {
        return Some(candidate_luau);
    }

    // Try init.lua / init.luau inside a directory
    let init_lua = cursor.join("init.lua");
    if init_lua.is_file() {
        return Some(init_lua);
    }
    let init_luau = cursor.join("init.luau");
    if init_luau.is_file() {
        return Some(init_luau);
    }

    // Also try src/init.lua / src/init.luau (common layout)
    let src_init_lua = cursor.join("src").join("init.lua");
    if src_init_lua.is_file() {
        return Some(src_init_lua);
    }
    let src_init_luau = cursor.join("src").join("init.luau");
    if src_init_luau.is_file() {
        return Some(src_init_luau);
    }

    None
}

/// Mutate a single thunk using filesystem resolution only (no sourcemap)
fn mutate_thunk_fs(path: &Path, packages_root: &Path) -> Result<()> {
    info!("Found link file '{}'", path.display());

    // Skip already-mutated thunks
    let original_src = std::fs::read_to_string(path)?;
    if original_src.contains("local REQUIRED_MODULE")
        || original_src.contains("return REQUIRED_MODULE")
    {
        info!("Link already mutated, leaving unchanged");
        return Ok(());
    }

    // Parse to extract the require() expression
    let parsed_code = match full_moon::parse(&original_src) {
        Ok(parsed_code) => parsed_code,
        Err(errors) => bail!(errors
            .iter()
            .map(|err| err.to_string())
            .collect::<Vec<_>>()
            .join("\n")),
    };

    if let Some(LastStmt::Return(r#return)) = parsed_code.nodes().last_stmt() {
        let returned_expression = r#return.returns().iter().next().unwrap();

        let path_components = match match_require(returned_expression) {
            Ok(components) => components,
            Err(err) => {
                warn!("Malformed link file, could not parse return expression, skipping. Run `wally install` to regenerate link files");
                error!("{:#}", err);
                return Ok(());
            }
        };

        info!(
            "Require expression converted to path: '{}'",
            path_components.join("/")
        );

        let file_path = resolve_components_fs(path, packages_root, &path_components)
            .context("Could not resolve require expression to file path via filesystem")?;
        let pass_through_contents =
            std::fs::read_to_string(&file_path).context("Failed to read linked file")?;

        // Build textual re-exports by scanning the target module for "export type" declarations.
        // We keep the original generic declaration on the LHS and emit RHS with generic names only,
        // including variadic generic packs (e.g., T...).
        let mut reexports = Vec::new();
        for line in pass_through_contents.lines() {
            let trimmed = line.trim_start();
            if !trimmed.starts_with("export type ") {
                continue;
            }

            // Parse "export type Name<...> = ..."
            let rest = &trimmed["export type ".len()..];

            // Extract type name and optional generics by locating the first '<' ... '>' pair.
            let (type_name, generics_opt) = if let Some(lt_rel) = rest.find('<') {
                let name = rest[..lt_rel].trim();
                // find the closing '>' after lt_rel
                let gt_rel = match rest[lt_rel + 1..].find('>') {
                    Some(i) => lt_rel + 1 + i,
                    None => continue,
                };
                let generics = &rest[lt_rel + 1..gt_rel];
                (name, Some(generics.trim()))
            } else {
                // No generics
                // Stop at the '=' if present, otherwise take the whole remainder
                let name_end = rest.find('=').unwrap_or(rest.len());
                (rest[..name_end].trim(), None)
            };

            // Derive list of generic parameter identifiers without defaults.
            // Keep variadic packs by preserving the trailing "...".
            let mut rhs_generics: Vec<String> = Vec::new();
            if let Some(generics) = generics_opt {
                for raw in generics.split(',') {
                    // Strip default, e.g., "S = T" -> "S" and preserve "..."
                    let name_only = raw.split('=').next().unwrap().trim();
                    if !name_only.is_empty() {
                        rhs_generics.push(name_only.to_string());
                    }
                }
            }

            // Rebuild generics for LHS (keep original) and RHS (names only)
            let lhs_suffix = if let Some(generics) = generics_opt {
                format!("<{}>", generics)
            } else {
                String::new()
            };
            let rhs_suffix = if rhs_generics.is_empty() {
                String::new()
            } else {
                format!("<{}>", rhs_generics.join(", "))
            };

            reexports.push(format!(
                "export type {}{} = REQUIRED_MODULE.{}{}",
                type_name, lhs_suffix, type_name, rhs_suffix
            ));
        }

        if reexports.is_empty() {
            info!("No exported types, leaving unchanged");
            return Ok(());
        }

        // Extract the original `require(...)` expression from the link source to preserve formatting.
        let mut require_expr_opt: Option<String> = None;
        for line in original_src.lines() {
            let t = line.trim_start();
            if t.starts_with("return require") {
                // Strip leading "return "
                let expr = t.strip_prefix("return ").unwrap_or(t).trim().to_string();
                require_expr_opt = Some(expr);
                break;
            }
        }
        // Build canonical one if missing
        let require_expr = require_expr_opt.unwrap_or_else(|| {
            let mut expr = String::from("require(");
            let mut first = true;
            for comp in &path_components {
                if first {
                    expr.push_str(comp);
                    first = false;
                } else if comp == "Parent" {
                    expr.push_str(".Parent");
                } else if comp == "_Index" {
                    expr.push_str("._Index");
                } else {
                    expr.push_str(&format!("[\"{}\"]", comp));
                }
            }
            expr.push(')');
            expr
        });

        let mut new_source = String::new();
        new_source.push_str(&format!("local REQUIRED_MODULE = {}\n", require_expr));
        for line in reexports {
            new_source.push_str(&line);
            new_source.push('\n');
        }
        new_source.push_str("return REQUIRED_MODULE\n");

        info!("Exported types found, writing new linker file");
        std::fs::write(path, new_source)?;
    } else {
        warn!("Malformed link file, no return statement found, skipping. Run `wally install` to regenerate link files");
        return Ok(());
    }

    Ok(())
}

fn handle_index_directory_fs(index_dir: &Path, packages_root: &Path) -> Result<bool> {
    let mut success = true;

    for package_entry in std::fs::read_dir(index_dir)?.flatten() {
        for thunk in std::fs::read_dir(package_entry.path())?.flatten() {
            if thunk.file_type().map(|t| t.is_file()).unwrap_or(false) {
                if let Err(err) = mutate_thunk_fs(&thunk.path(), packages_root) {
                    error!("{:#}", err);
                    success = false;
                }
            }
        }
    }

    Ok(success)
}

/// Public entry point: filesystem-based traversal (no sourcemap needed)
pub fn run_on_packages_fs(packages_root: &Path) -> Result<()> {
    let mut success = true;

    // Iterate top-level under Packages
    for entry in std::fs::read_dir(packages_root)
        .with_context(|| format!("Failed to read packages folder {}", packages_root.display()))?
        .flatten()
    {
        if entry.file_name() == "_Index" {
            match handle_index_directory_fs(&entry.path(), packages_root) {
                Ok(ok) => success &= ok,
                Err(err) => {
                    error!("{:#}", err);
                    success = false;
                }
            }
            continue;
        }

        // Only mutate link thunks (files)
        if entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            if let Err(err) = mutate_thunk_fs(&entry.path(), packages_root) {
                error!("{:#}", err);
                success = false;
            }
        }
    }

    if success {
        Ok(())
    } else {
        bail!("Mutation did not complete successfully")
    }
}
