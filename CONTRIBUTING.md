# Contributing to snypr

## Commits

[Conventional Commits](https://www.conventionalcommits.org/), enforced by
commitizen through the prek hooks. The changelog and the next version number
are derived from them, so the type and scope matter:

```
feat(selector): pre-select the focused monitor
fix(overlay): keep the veil visible while annotating
docs(man): document --clipboard-type
```

## Adding a bundled icon

Snypr ships a small set of [icon-development-kit](https://gitlab.gnome.org/Teams/Design/icon-development-kit)
SVGs via a gresource so that toolbar buttons render reliably regardless of the
user's icon theme. To add one more:

1. Pick the icon you want from the [GNOME Icon Library](https://flathub.org/apps/org.gnome.design.IconLibrary)
   app — every icon shown there is CC0 1.0 and safe to vendor here under MIT.
2. Save the SVG to `data/icons/scalable/actions/<name>-symbolic.svg`.
   - Use the **freedesktop** name (e.g. `document-save-symbolic`). GTK will then
     resolve `Image::from_icon_name("document-save-symbolic")` to your file
     before falling back to the system theme.
3. Add one `<file>` line to `data/icons.gresource.xml`:

   ```xml
   <file alias="<name>-symbolic.svg" preprocess="xml-stripblanks">icons/scalable/actions/<name>-symbolic.svg</file>
   ```

4. `cargo build` — `build.rs` recompiles the gresource automatically and
   `cargo:rerun-if-changed=...` directives ensure the SVG change is picked up.
5. No Rust code changes needed: any existing `Image::from_icon_name("<name>-symbolic")`
   call now picks up the vendored asset.

The vendored icons are CC0; attribution lives in `data/icons/LICENSE.md` and
the README `Acknowledgements` section.

## Releases

Orchestrated by [gh-ship](https://github.com/noirbizarre/gh-ship), which is why
commit messages matter: `git cliff` derives both the changelog and the next
version number from them.

The lifecycle is:

1. push to `main` → `gh ship prepare` opens or updates the **Release PR**,
   carrying the version bump and the changelog;
2. review the changelog and merge it;
3. `gh ship release` tags the merge commit, drafts the release, attaches the
   assets, and only then makes it public;
4. publishing the release triggers `aur.yml`, which pushes the three AUR
   packages.

Maintainers do not tag by hand. `gh ship validate` runs in CI, so a workflow
that stops satisfying the contract fails on a pull request rather than
mid-release.

The release workflows authenticate as a GitHub App, whose `APP_CLIENT_ID`
variable and `APP_PRIVATE_KEY` secret live in the `release` environment. The
default `GITHUB_TOKEN` is not enough: a Release PR it authored would show no CI
results, because pushes made with it do not trigger workflows.

### Assets

`publish-release.yml` produces two tarballs:

- a **prebuilt binary** tarball, built inside an `archlinux:base-devel`
  container. Snypr links GTK4 and gtk4-layer-shell dynamically, so this binary
  is only supported on Arch — it exists so `snypr-bin` users never compile;
- a **source tarball** from `git archive`, which is what `snypr`, `snypr-git`
  and other distribution packagers consume.

### AUR

The PKGBUILD templates live in `packaging/aur/`. `snypr` and `snypr-bin` carry
`@VERSION@` and `@SHA256@` placeholders that `aur.yml` substitutes from the
published release assets; `snypr-git` derives both itself.

`aur.yml` runs on `release: published` rather than from `publish-release`,
because gh-ship only undrafts the release once that workflow succeeds — waiting
means the URLs baked into the PKGBUILDs already resolve. It builds `snypr-bin`
and `snypr` with `makepkg --nocheck` before pushing, so a broken PKGBUILD fails
in CI rather than on a user's machine. `snypr-git` is skipped: its source is the
branch tip rather than the release, so a CI build would not describe what users
get. `check()` is skipped everywhere because it re-runs, in release mode, the
suite CI already ran on the same commit.

Pushing requires an `AUR_SSH_PRIVATE_KEY` secret in the dedicated `aur`
environment, whose public half is registered on the maintainer's
aur.archlinux.org account. It is deliberately not in `release`: the AUR key has
no business sitting next to the GitHub App credentials, and a separate
environment makes each package push show up as its own deployment.

The AUR creates a repository on its **first** push, so each pkgbase has to be
bootstrapped manually once:

```sh
git clone ssh://aur@aur.archlinux.org/snypr-bin.git
cd snypr-bin
sed -e "s/@VERSION@/$VERSION/" -e "s/@SHA256@/$SHA256/" \
  ../packaging/aur/snypr-bin/PKGBUILD > PKGBUILD
makepkg --printsrcinfo > .SRCINFO
git add PKGBUILD .SRCINFO && git commit -m "Initial import" && git push
```

CI owns every subsequent update.

Before touching a PKGBUILD, lint it locally with `namcap PKGBUILD` and build it
with `makepkg -s`.

Useful locally:

| Command | Description |
|---|---|
| `mise run version` | show the version the next release would carry |
| `mise run changelog` | preview `CHANGELOG.md` for the next release |
| `mise run dogfood` | `gh ship validate` — check the release setup |
| `mise run lint:actions` | lint the workflow files with actionlint |
| `mise run spell` | spellcheck with `typos` (also run by `git-cliff` at release time) |
