#!/usr/bin/env bash
#
# Cut an Actus release.
#
#   ./scripts/release.sh minor        # 1.0.1 → 1.1.0
#   ./scripts/release.sh patch        # 1.0.1 → 1.0.2
#   ./scripts/release.sh 2.0.0-rc.1   # explicit, for what arithmetic can't say
#
# Bumps the workspace version, cuts the CHANGELOG, runs the full gate, commits,
# tags, and pushes. Pushing the tag is what publishes: `.github/workflows/
# release.yml` fires on `v*` and uploads to crates.io via Trusted Publishing.
#
# Nothing here needs crates.io credentials — no `cargo login`, no token on any
# laptop. The only thing this script sends anywhere is a git push.
#
# Flags:
#   --yes        don't ask before the push (unattended use)
#   --no-push    do everything locally and stop; prints the push commands
#
set -euo pipefail

# ─── output ──────────────────────────────────────────────────────────────────
if [[ -t 1 ]]; then
  R=$'\033[31m'; G=$'\033[32m'; Y=$'\033[33m'; B=$'\033[1m'; D=$'\033[2m'; X=$'\033[0m'
else
  R=''; G=''; Y=''; B=''; D=''; X=''
fi
_info() { printf '%s\n' "$*"; }
_step() { printf '\n%s▸ %s%s\n' "$B" "$*" "$X"; }
_ok()   { printf '%s✓%s %s\n' "$G" "$X" "$*"; }
_warn() { printf '%s!%s %s\n' "$Y" "$X" "$*" >&2; }
_die()  { printf '%s✗ %s%s\n' "$R" "$*" "$X" >&2; exit 1; }

# ─── args ────────────────────────────────────────────────────────────────────
# The argument is normally a bump kind — major / minor / patch — and the version
# is computed from what the manifest currently carries. An explicit semver is
# still accepted for the cases arithmetic can't express (a prerelease, or
# skipping a version deliberately).
SPEC=""; ASSUME_YES=0; PUSH=1
while (($#)); do
  case "$1" in
    --yes|-y)  ASSUME_YES=1 ;;
    --no-push) PUSH=0 ;;
    -h|--help) sed -n '2,22p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    -*)        _die "unknown flag: $1" ;;
    *)         [[ -z "$SPEC" ]] || _die "unexpected argument: $1"; SPEC="$1" ;;
  esac
  shift
done
[[ -n "$SPEC" ]] || _die "usage: ./scripts/release.sh <major|minor|patch|X.Y.Z> [--yes] [--no-push]"
case "$SPEC" in
  major|minor|patch) ;;
  *) [[ "$SPEC" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]] \
       || _die "'$SPEC' is neither a bump kind (major|minor|patch) nor a semver version" ;;
esac

cd "$(dirname "$0")/.."
[[ -f crates/actus/Cargo.toml ]] || _die "not in the actus repo root"

# ═════════════════════════════════════════════════════════════════════════════
# PREFLIGHT — everything that can refuse, before anything mutates.
# ═════════════════════════════════════════════════════════════════════════════
_step "Preflight"

# Read the current version from cargo itself rather than by grepping the
# manifest: authoritative, and immune to the manifest's layout changing.
_meta() {
  cargo metadata --no-deps --format-version 1 2>/dev/null | python3 -c "
import json, sys
pkg = next(p for p in json.load(sys.stdin)['packages'] if p['name'] == 'actus')
print(pkg.get('$1') or '')
"
}
CUR="$(_meta version)" || _die "cargo metadata failed — does the manifest parse?"
[[ -n "$CUR" ]] || _die "could not read the workspace version from cargo metadata"

# Resolve the bump kind against the current version. Any prerelease suffix is
# dropped before the arithmetic: 1.2.0-rc.1 patch → 1.2.1, not 1.2.0-rc.2.
if [[ "$SPEC" =~ ^(major|minor|patch)$ ]]; then
  IFS=. read -r _ma _mi _pa <<<"${CUR%%-*}"
  case "$SPEC" in
    major) NEW="$((_ma + 1)).0.0" ;;
    minor) NEW="$_ma.$((_mi + 1)).0" ;;
    patch) NEW="$_ma.$_mi.$((_pa + 1))" ;;
  esac
  _info "${D}$SPEC bump: $CUR → $NEW${X}"
else
  NEW="$SPEC"
fi
TAG="v$NEW"

git diff --quiet && git diff --cached --quiet \
  || _die "working tree is dirty — commit or stash first"

BRANCH="$(git rev-parse --abbrev-ref HEAD)"
DEFAULT="$(git symbolic-ref --quiet --short refs/remotes/origin/HEAD 2>/dev/null | sed 's|^origin/||' || true)"
DEFAULT="${DEFAULT:-main}"
[[ "$BRANCH" == "$DEFAULT" ]] \
  || _die "on '$BRANCH', not '$DEFAULT' — releases are cut from the default branch"

_info "${D}fetching origin…${X}"
git fetch --quiet origin --tags
[[ "$(git rev-parse HEAD)" == "$(git rev-parse "origin/$DEFAULT")" ]] \
  || _die "$BRANCH is not in sync with origin/$DEFAULT — pull/push first"

# Version must move forward. `sort -V` puts the lower one first; equal versions
# fail the second test, so a re-release of the current version is refused.
[[ "$CUR" != "$NEW" ]] || _die "version is already $CUR"
[[ "$(printf '%s\n%s\n' "$CUR" "$NEW" | sort -V | head -1)" == "$CUR" ]] \
  || _die "$NEW is older than the current $CUR"

git rev-parse -q --verify "refs/tags/$TAG" >/dev/null && _die "tag $TAG already exists locally"
git ls-remote --exit-code --tags origin "$TAG" >/dev/null 2>&1 && _die "tag $TAG already exists on origin"

# The CHANGELOG's [Unreleased] section must actually say something. Releasing an
# empty section is a silent way to ship a version nobody can read about.
UNREL="$(perl -ne 'print if /^## \[Unreleased\]/../^## \[(?!Unreleased)/' CHANGELOG.md \
          | sed '1d;$d' | grep -v '^\s*$' || true)"
[[ -n "$UNREL" ]] || _die "CHANGELOG.md has nothing under [Unreleased] — write the entry first"

_ok "$CUR → ${B}$NEW${X} on $BRANCH, tree clean, in sync with origin, $TAG is free"

# ═════════════════════════════════════════════════════════════════════════════
# BUMP — Cargo.toml, CHANGELOG.md, Cargo.lock.
#
# Every edit is asserted to have landed. A regex that silently matches nothing
# would otherwise produce a "release" commit that changed no version at all.
# ═════════════════════════════════════════════════════════════════════════════
_step "Bumping $CUR → $NEW"

# (1) [workspace.package] version — the only bare `version = "…"` at line start.
perl -pi -e "s{^version = \"\Q$CUR\E\"$}{version = \"$NEW\"}" Cargo.toml
# (2) The five internal dep pins, identified by `path = "crates/…"` on the line.
perl -pi -e "s{(path = \"crates/[^\"]*\", version = )\"\Q$CUR\E\"}{\${1}\"$NEW\"}" Cargo.toml

BUMPED="$(grep -c "\"$NEW\"" Cargo.toml || true)"
(( BUMPED == 6 )) || _die "expected 6 version edits in Cargo.toml (1 workspace + 5 internal pins), made $BUMPED — the manifest layout changed; fix this script"
_ok "Cargo.toml — workspace version + 5 internal pins"

# (3) CHANGELOG: open a fresh [Unreleased], retitle the old one to the version.
perl -0pi -e "s{^## \[Unreleased\]\n}{## [Unreleased]\n\n## [$NEW]\n}m" CHANGELOG.md
# (4) Bottom link refs: repoint [Unreleased] and add a compare link for $NEW.
perl -pi -e "s{^\[Unreleased\]: (.*)/compare/v\Q$CUR\E\.\.\.HEAD\$}{[Unreleased]: \${1}/compare/v$NEW...HEAD\n[$NEW]: \${1}/compare/v$CUR...v$NEW}" CHANGELOG.md

grep -q "^## \[$NEW\]$" CHANGELOG.md || _die "CHANGELOG heading for $NEW was not written — format drifted; fix this script"
grep -q "^\[$NEW\]: " CHANGELOG.md   || _die "CHANGELOG link ref for $NEW was not written — format drifted; fix this script"
grep -q "^\[Unreleased\]: .*v$NEW\.\.\.HEAD$" CHANGELOG.md || _die "[Unreleased] link was not repointed to $NEW"
_ok "CHANGELOG.md — [Unreleased] cut to [$NEW], links repointed"

cargo update --workspace --quiet
git diff --quiet -- Cargo.lock && _warn "Cargo.lock unchanged (unexpected, but not fatal)" || _ok "Cargo.lock refreshed"

# ═════════════════════════════════════════════════════════════════════════════
# GATE — the same checks ci.yml runs, both feature configs, plus the packaging
# proof. CI runs these again on the tag; this is so you learn here, not there.
# ═════════════════════════════════════════════════════════════════════════════
ALL_FEATURES="actus/compression,actus/websocket,actus/openapi"

_step "Gate: rustfmt"
cargo fmt --all -- --check
_ok "fmt"

_step "Gate: clippy (default features)"
cargo clippy --all-targets -- -D warnings
_step "Gate: clippy (all features)"
cargo clippy --all-targets --features "$ALL_FEATURES" -- -D warnings
_ok "clippy, both configs"

_step "Gate: tests (default features)"
cargo test --workspace
_step "Gate: tests (all features)"
cargo test --workspace --features "$ALL_FEATURES"
_ok "tests, both configs"

# MSRV mirrors ci.yml's floor. Skipped (not failed) when the toolchain is absent
# — CI is the authority on it; this is a local convenience.
MSRV="$(_meta rust_version)"
if [[ -n "$MSRV" ]] && rustup toolchain list 2>/dev/null | grep -q "^$MSRV"; then
  _step "Gate: MSRV ($MSRV)"
  cargo "+$MSRV" check --workspace --all-targets
  _ok "MSRV $MSRV"
else
  _warn "MSRV toolchain $MSRV not installed — skipped locally (CI enforces it). Install: rustup toolchain install $MSRV"
fi

# The authoritative packaging check: builds each crate from its *packaged*
# tarball against a temp registry, in dependency order. This is what catches a
# missing `version` on an internal dep or a file the package excludes.
#
# --allow-dirty because the bump above is deliberately not committed yet: the
# gate runs first so that a failure never leaves a commit to clean up. Preflight
# guaranteed the tree was clean, so the only uncommitted files are the three
# this script just wrote — i.e. exactly the state about to be committed.
_step "Gate: cargo publish --workspace --dry-run"
cargo publish --workspace --dry-run --allow-dirty
_ok "packaging verified for all five crates"

# ═════════════════════════════════════════════════════════════════════════════
# COMMIT · TAG · PUSH
# ═════════════════════════════════════════════════════════════════════════════
_step "Committing $TAG"

git add Cargo.toml Cargo.lock CHANGELOG.md
git commit --quiet --file - <<MSG
chore(release): bump workspace to $NEW; cut CHANGELOG $NEW

Released from $DEFAULT. The tag push triggers .github/workflows/release.yml,
which gates again and publishes all five crates to crates.io via Trusted
Publishing (no stored token).
MSG
git tag -a "$TAG" -m "Actus $NEW"
_ok "committed and tagged $TAG"

if (( ! PUSH )); then
  _step "Stopped before push (--no-push)"
  _info "Nothing has left this machine. To release:"
  _info "  ${B}git push origin $DEFAULT && git push origin $TAG${X}"
  _info "To undo: ${B}git tag -d $TAG && git reset --hard HEAD~1${X}"
  exit 0
fi

printf '\n%sPushing %s publishes %s to crates.io. Versions there are permanent —\nthey can be yanked, never deleted or replaced.%s\n' "$Y" "$TAG" "$NEW" "$X"
if (( ! ASSUME_YES )); then
  read -r -p "Type 'yes' to push: " reply
  if [[ "$reply" != "yes" ]]; then
    _info "${D}cancelled — the commit and tag are still here, unpushed${X}"
    _info "${D}undo with: git tag -d $TAG && git reset --hard HEAD~1${X}"
    exit 1
  fi
fi

_step "Pushing"
git push --quiet origin "$DEFAULT"
git push --quiet origin "$TAG"
_ok "pushed $DEFAULT and $TAG"

REPO="$(git remote get-url origin | sed -E 's|.*github\.com[:/]||; s|\.git$||')"
printf '\n%s✓ %s is on its way.%s\n' "$G" "$TAG" "$X"
_info "  Watch:  https://github.com/$REPO/actions"
_info "  Lands:  https://crates.io/crates/actus/$NEW"
