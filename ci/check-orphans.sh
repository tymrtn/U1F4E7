#!/usr/bin/env bash
# ci/check-orphans.sh — detect orphaned Rust source files.
#
# For each `crates/*/src/` directory, walk every `.rs` file and verify it is
# declared via `mod X;` or `pub mod X;` somewhere in the same crate. Files
# that are exempt (crate roots, module barrels, binaries, tests, examples,
# integration helpers) are skipped.
#
# Exit 0 if all files are wired up; exit 1 with a listing of orphans otherwise.
#
# Rationale: orphaned `.rs` files don't break `cargo build` — the compiler
# simply never looks at them. Entire features have silently vanished this way.
# This script is the first line of defense.

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

# Files that are allowed to exist without a `mod` declaration.
declare -a EXEMPT_BASENAMES=(
    "lib.rs"
    "main.rs"
    "mod.rs"
    "build.rs"
)

is_exempt() {
    local basename="$1"
    for exempt in "${EXEMPT_BASENAMES[@]}"; do
        if [[ "$basename" == "$exempt" ]]; then
            return 0
        fi
    done
    return 1
}

orphans_found=0
orphans_list=()

# Find every crate under crates/
for crate_src in crates/*/src; do
    [[ -d "$crate_src" ]] || continue
    crate_name="$(basename "$(dirname "$crate_src")")"

    # Walk every .rs file in src/, excluding bin/ (binary targets have their
    # own [[bin]] section in Cargo.toml) and tests/ (integration tests).
    while IFS= read -r -d '' rs_file; do
        rel_path="${rs_file#$repo_root/}"
        basename="$(basename "$rs_file")"
        stem="${basename%.rs}"

        # Skip exempt files
        if is_exempt "$basename"; then
            continue
        fi

        # Skip files inside bin/ or tests/ subdirectories
        dir_path="$(dirname "$rs_file")"
        if [[ "$dir_path" == *"/bin" || "$dir_path" == *"/bin/"* ]]; then
            continue
        fi
        if [[ "$dir_path" == *"/tests" || "$dir_path" == *"/tests/"* ]]; then
            continue
        fi

        # Search for `mod <stem>;` or `pub mod <stem>;` anywhere in the crate.
        # A file at crates/foo/src/bar/baz.rs needs `mod baz;` somewhere
        # reachable from the crate root — we approximate by grepping the
        # whole crate for any `mod baz;` or `pub mod baz;` declaration.
        if ! grep -rEq "^[[:space:]]*(pub[[:space:]]+)?mod[[:space:]]+${stem}[[:space:]]*;" "$crate_src" 2>/dev/null; then
            orphans_list+=("$rel_path")
            orphans_found=1
        fi
    done < <(find "$crate_src" -type f -name '*.rs' -print0)
done

if [[ $orphans_found -eq 1 ]]; then
    echo "ERROR: orphaned Rust source files found (no mod declaration):"
    printf '  - %s\n' "${orphans_list[@]}"
    echo ""
    echo "Fix: add 'pub mod <name>;' or 'mod <name>;' to the crate root or a parent mod.rs"
    echo "Or: delete the file if it's unused."
    exit 1
fi

echo "ci/check-orphans.sh: all Rust source files are wired up ✓"
exit 0
