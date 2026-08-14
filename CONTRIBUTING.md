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
   source tarball and its checksums, and only then makes it public.

Maintainers do not tag by hand. `gh ship validate` runs in CI, so a workflow
that stops satisfying the contract fails on a pull request rather than
mid-release.

The release workflows authenticate as a GitHub App, whose `APP_CLIENT_ID`
variable and `APP_PRIVATE_KEY` secret live in the `release` environment. The
default `GITHUB_TOKEN` is not enough: a Release PR it authored would show no CI
results, because pushes made with it do not trigger workflows.

Releases carry **no prebuilt binaries**: snypr links GTK4 and
gtk4-layer-shell dynamically and installs desktop entries, icons and a manpage,
so a single binary would only work on distributions matching the CI runner.
The release asset is a source tarball, which is what packagers consume.

Useful locally:

| Command | Description |
|---|---|
| `mise run version` | show the version the next release would carry |
| `mise run changelog` | preview `CHANGELOG.md` for the next release |
| `mise run dogfood` | `gh ship validate` — check the release setup |
| `mise run lint:actions` | lint the workflow files with actionlint |
