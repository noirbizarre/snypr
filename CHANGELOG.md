# Changelog

All notable changes to this project will be documented in this file.

This project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.3.0](https://github.com/noirbizarre/snypr/compare/0.2.0..0.3.0) - 2026-08-18

### 💫 Features

- **ui** Switch the output destination from the toolbar ([#24](https://github.com/noirbizarre/snypr/issues/24)) - ([b08cdf1](https://github.com/noirbizarre/snypr/commit/b08cdf1b11f2dbc1942d8e2ce385984ed52fac6c))

### 📚 Documentation

- Add badges to the README header ([#26](https://github.com/noirbizarre/snypr/issues/26)) - ([a31d0c9](https://github.com/noirbizarre/snypr/commit/a31d0c954f52b377f44e93c79b37167a49ca8dcd))

### 🧪 Tests

- **ui** Cover the output-destination switcher ([#25](https://github.com/noirbizarre/snypr/issues/25)) - ([30ae7d4](https://github.com/noirbizarre/snypr/commit/30ae7d4e41b43eaefd70185e40998e6547d19029))

### 🏗️ Build

- **deps** Bump serde from 1.0.228 to 1.0.229 ([#18](https://github.com/noirbizarre/snypr/issues/18)) - ([ac81012](https://github.com/noirbizarre/snypr/commit/ac81012331bde4e230eded73a243ba66df47c29e))
- **deps** Bump memmap2 from 0.9.10 to 0.9.11 ([#21](https://github.com/noirbizarre/snypr/issues/21)) - ([b00786c](https://github.com/noirbizarre/snypr/commit/b00786c024066de00b7dfaeabd30ec0b76687226))
- **deps** Bump serde_json from 1.0.149 to 1.0.151 ([#22](https://github.com/noirbizarre/snypr/issues/22)) - ([5c32cd8](https://github.com/noirbizarre/snypr/commit/5c32cd8ebae04350dc388f5e646f35ab3ac4b9b0))
- **deps** Bump tokio from 1.52.3 to 1.53.1 ([#20](https://github.com/noirbizarre/snypr/issues/20)) - ([2f543ed](https://github.com/noirbizarre/snypr/commit/2f543ed91dc170e3c2543e233d1dde831a6e3e39))
- **deps** Bump gtk4 from 0.11.3 to 0.11.4 ([#19](https://github.com/noirbizarre/snypr/issues/19)) - ([2cee62c](https://github.com/noirbizarre/snypr/commit/2cee62c073b6f4fee0a9124cdff223aa84f3d7c0))

## [0.2.0](https://github.com/noirbizarre/snypr/compare/0.1.0..0.2.0) - 2026-08-15

### 💫 Features

- **capture** Honor [capture] cursor as the --cursor default - ([46c920a](https://github.com/noirbizarre/snypr/commit/46c920a0d5ccdef8f416986393ca7aa35556df1b))
- **config** Honor --config in every subcommand - ([dd10d1d](https://github.com/noirbizarre/snypr/commit/dd10d1db09e6e47e412d65df161cb0212b613fa3))

### 🐛 Bug Fixes

- **aur** Compute the real pkgver for the VCS package - ([f591da1](https://github.com/noirbizarre/snypr/commit/f591da1fa3a9d424f16640d8900955c1095a9554))
- **aur** Resolve SSH paths and the idempotency guard - ([285434b](https://github.com/noirbizarre/snypr/commit/285434b5e41a5c91d1daa7986a2ab8cfdb4fca49))
- **packaging** Ship a scalable icon and the 48x48 size - ([46f3e74](https://github.com/noirbizarre/snypr/commit/46f3e74b916f66dec1d8de237eb0209e389d99ad))
- **ui** Surface overlay failures to the user - ([f2ee0ed](https://github.com/noirbizarre/snypr/commit/f2ee0ede615e9f1a529914e3495c5acf0f6b2607))
- **ui** Notify on draw-overlay save - ([4de2caf](https://github.com/noirbizarre/snypr/commit/4de2caf63231cd42ea1b517bbe6408568f74a2ff))
- **ui** Report draw-save selector failures at error level - ([ce6ac05](https://github.com/noirbizarre/snypr/commit/ce6ac0537004e1f5dcd6cd243b964c61f4482eb9))
- Unify mutex poison handling for the written-paths list - ([01718a1](https://github.com/noirbizarre/snypr/commit/01718a19340ed9f0c16c92bb00f8aec729d5d1e3))

### ⚡ Performance

- **cli** Reuse the fast swizzle for the editor's document base - ([26d5e7c](https://github.com/noirbizarre/snypr/commit/26d5e7c66b2fc64e5eb9601c8a86e6dbafe3b64a))

### 🔨 Refactor

- **annotate** Share rect hit_test and add the missing constructors - ([68ee50f](https://github.com/noirbizarre/snypr/commit/68ee50f26e21ae831d5185f9df108e7463ea5142))
- **config** Drop the unimplemented [keybinds] section - ([64d2f5c](https://github.com/noirbizarre/snypr/commit/64d2f5cd21bc5ed1e60a1c7d332e141cf60dd3a2))
- **i18n** Translate the remaining user-facing errors - ([6fdddb8](https://github.com/noirbizarre/snypr/commit/6fdddb8b844f49b4942b8ef4010ce1fd533cfe5c))
- **ui** Share the save pipeline between both save routes - ([cffbcf4](https://github.com/noirbizarre/snypr/commit/cffbcf42c823c1442143c73d520e3ca19c1e4437))
- **ui** Share the GTK monitor prologue and exit check - ([e35d4e8](https://github.com/noirbizarre/snypr/commit/e35d4e810b25bb200053985bc5266c654b46d05f))

### 📚 Documentation

- Correct the aur.yml, coverage and tooling claims - ([df91ffe](https://github.com/noirbizarre/snypr/commit/df91ffe08b9bec5b19252f6a9a4ca3d45d2aad8b))
- Sync the packager table with the PKGBUILD install lists - ([8921e0d](https://github.com/noirbizarre/snypr/commit/8921e0d090f6732035ef106c1f1b7ccae3f9f642))
- Fix stale README and manpage references - ([6239bce](https://github.com/noirbizarre/snypr/commit/6239bce16dd32fef3c252ee24c45712bc3a9250d))
- Document the SNYPR_* environment variables - ([25aaf2a](https://github.com/noirbizarre/snypr/commit/25aaf2ab7dcc64e529cc3be4df2c75330419963e))
- Document --lang, language, --socket, --passthrough and -i - ([54d11cc](https://github.com/noirbizarre/snypr/commit/54d11ccba0eba5ded99ad282924fdec6b1a2fd44))
- Fix tray, tool count and blur availability claims - ([d760886](https://github.com/noirbizarre/snypr/commit/d7608867f3c17928a171147c930fa37a44d264c0))
- Correct --window and --focused descriptions - ([c8fc2c3](https://github.com/noirbizarre/snypr/commit/c8fc2c315c4eae0068182b7e34363dc4caf3dab4))

### 🧪 Tests

- Cover the new constructors, save pipeline and exit check - ([52d793a](https://github.com/noirbizarre/snypr/commit/52d793a38f1e442c2879b43261e9db78d7ed9149))
- Cover the pure capture, config and protocol logic ([#16](https://github.com/noirbizarre/snypr/issues/16)) - ([1be4eea](https://github.com/noirbizarre/snypr/commit/1be4eea8259fe6b708a608ffccfdd5dfdfacd943))

### 🎨 Style

- Apply prek formatting - ([d852d95](https://github.com/noirbizarre/snypr/commit/d852d95c96fd06c64df8b71aea7edc0ec5bd5c46))

### 🏗️ Build

- **deps** Bump pango from 0.22.6 to 0.22.8 ([#14](https://github.com/noirbizarre/snypr/issues/14)) - ([7bac886](https://github.com/noirbizarre/snypr/commit/7bac88630395d1673ae11ec0356b296e074d064c))
- **deps** Bump async-trait from 0.1.89 to 0.1.92 ([#15](https://github.com/noirbizarre/snypr/issues/15)) - ([b58b94b](https://github.com/noirbizarre/snypr/commit/b58b94b7d473800fe98e6513f314f23976156044))
- **deps** Bump gtk4-layer-shell from 0.8.0 to 0.8.1 ([#11](https://github.com/noirbizarre/snypr/issues/11)) - ([6fc5b53](https://github.com/noirbizarre/snypr/commit/6fc5b53bf152e17d2ed6aeda38ff8d9c2feba7f9))
- **deps** Bump gsk4 from 0.11.1 to 0.11.4 ([#13](https://github.com/noirbizarre/snypr/issues/13)) - ([a7ca3db](https://github.com/noirbizarre/snypr/commit/a7ca3dbcac9832b1ba55387647971fea6422fd64))
- **deps** Bump wayland-client from 0.31.14 to 0.31.15 ([#12](https://github.com/noirbizarre/snypr/issues/12)) - ([da78b29](https://github.com/noirbizarre/snypr/commit/da78b290cff3dc1fadcdcb95ec021f95b48789b4))
- **packaging** Depend on desktop-file-utils - ([d889b0b](https://github.com/noirbizarre/snypr/commit/d889b0baa4f3bfb03f52b979c07b8b7b60fa248b))
- Make the typos check pass - ([794aca5](https://github.com/noirbizarre/snypr/commit/794aca5ef11e95a01fbb197fa8edcffd82672197))

### 🔧 CI

- Bump actions to their latest major and let Dependabot track them ([#10](https://github.com/noirbizarre/snypr/issues/10)) - ([96120e3](https://github.com/noirbizarre/snypr/commit/96120e345f2e7edac258b08f9a7383d0753ef3a5))

## 0.1.0 - 2026-08-14

### 💫 Features

- **annotate** Select, move, resize and re-edit shapes - ([5c7b3e4](https://github.com/noirbizarre/snypr/commit/5c7b3e44e431cf3f053cd2ba0f0270f66e2ac17d))
- **annotate** Constrain rect/ellipse to square/circle while SHIFT is held - ([64959d1](https://github.com/noirbizarre/snypr/commit/64959d1a828722468066760063044857aacc4a22))
- **annotate** Reverse blur and draw-mode blur - ([29a23af](https://github.com/noirbizarre/snypr/commit/29a23af7b73e8a4e9f4a5b61aab4c42d5bbcd441))
- **annotate** WYSIWYG text tool with live in-canvas editor - ([d256d3e](https://github.com/noirbizarre/snypr/commit/d256d3e9b0d79fca26370134f6fbd57e8294a904))
- **annotate** Add Line tool - ([4f07731](https://github.com/noirbizarre/snypr/commit/4f077317e8b25c48db3fa996f4e6dbb3a5159abc))
- **annotate** Add ellipse tool - ([6c03fc3](https://github.com/noirbizarre/snypr/commit/6c03fc3606ca671dac7f5bbed12f14e8db4fad80))
- **annotate** Land Text and Blur tools - ([ee8ca41](https://github.com/noirbizarre/snypr/commit/ee8ca4100c08a00f862b4bbac08fb765c67be6a7))
- **annotate** Add highlight, freehand, number, redact, and crop tools - ([9c68242](https://github.com/noirbizarre/snypr/commit/9c68242b40ad164705ba3e4dd2a8b93fbe60924c))
- **annotate** Cairo-backed canvas with Rect/Arrow tools and save - ([eb4d091](https://github.com/noirbizarre/snypr/commit/eb4d091dbe84fe08724578b17890ba8149af8cfd))
- **capture** Add configurable pre-capture delay with visual countdown - ([94afee5](https://github.com/noirbizarre/snypr/commit/94afee517c64cc8632df085a0ad530517ad79f50))
- **capture** Wire selector → wlr capture → editor → sinks - ([d4899f4](https://github.com/noirbizarre/snypr/commit/d4899f474708d1c795f627727fd81eb8c32270fc))
- **cli** Add `doctor` subcommand for diagnostic reports - ([954ab3c](https://github.com/noirbizarre/snypr/commit/954ab3c7dfc36257632b5cf45462d239a0ca5740))
- **cli** Add --to / --cursor flags to draw - ([6b0228f](https://github.com/noirbizarre/snypr/commit/6b0228f15396e737453c98066f5e0b46a5183037))
- **cli**  🚨 **breaking** Scope --via-daemon to clients; add daemon --systray - ([4cd37c3](https://github.com/noirbizarre/snypr/commit/4cd37c32250a19165ebccf71ca367ea6a878c2fe))
- **cli**  🚨 **breaking** Fold capture into screenshot --edit - ([04d6760](https://github.com/noirbizarre/snypr/commit/04d6760d2990c62c2948c920f3365f509bccec3c))
- **cli** Write one file per output for --per-output - ([ef6c5fd](https://github.com/noirbizarre/snypr/commit/ef6c5fd4191fd0dcb5f49a22d035f508e534dca8))
- **cli** Resolve Window/Focused via Hyprland IPC before capture - ([3aee39b](https://github.com/noirbizarre/snypr/commit/3aee39b37637ac143766b631e54fa086b9aa77ae))
- **clipboard** Persist selection after CLI exits and add --clipboard-type - ([eefcba6](https://github.com/noirbizarre/snypr/commit/eefcba6a1bfb50f54defa762a4f364b673852832))
- **config** Make selector initial mode and annotation tool colors configurable - ([38f73bb](https://github.com/noirbizarre/snypr/commit/38f73bb0e7a3d6e45229292487b8d5dbe569fd25))
- **daemon** Capture and draw toggle over IPC - ([a67590a](https://github.com/noirbizarre/snypr/commit/a67590acffc704a04ae714e08f8c7cab9a5b29d9))
- **daemon** Dispatch screenshot requests over IPC - ([98d4f29](https://github.com/noirbizarre/snypr/commit/98d4f29c8d1d26d6d798676c110b132137656b95))
- **i18n** Introduce Fluent-backed translations with French support - ([1801e4b](https://github.com/noirbizarre/snypr/commit/1801e4be450273e7fa65682923bdcb3af373afcf))
- **notify** Shorten saved-path display in success notifications - ([b6244cd](https://github.com/noirbizarre/snypr/commit/b6244cd643bb6bb2f364f6175d3f3ef68a979d07))
- **notify** Success notification with screenshot thumbnail - ([4a506c3](https://github.com/noirbizarre/snypr/commit/4a506c3eb745f08a3b45eab676ef1dc21129abf9))
- **output** Configurable PNG compression preset - ([b88cdf3](https://github.com/noirbizarre/snypr/commit/b88cdf3d5b703c76e5c54c4b6d7d58d263331e70))
- **packaging** Ship prebuilt binaries and publish to the AUR - ([f47bf18](https://github.com/noirbizarre/snypr/commit/f47bf1856e10b78f9a65f86f04be023a485ae779))
- **packaging** Add desktop entries and multi-size icons - ([546b0d3](https://github.com/noirbizarre/snypr/commit/546b0d3a2fd78be53882e1cfba10dcd26386eedb))
- **selector** Make the region selection resizable and movable - ([29d73a5](https://github.com/noirbizarre/snypr/commit/29d73a58cc4e5fd985e57d33ff9047b11fd01c40))
- **selector** Add outline_hover color for hovered zones - ([b194b99](https://github.com/noirbizarre/snypr/commit/b194b99adddf19885a9a4292429167c85ec939d4))
- **selector** Make chrome colors configurable via [ui.selector] - ([a1bfc12](https://github.com/noirbizarre/snypr/commit/a1bfc12ac550aee0134a32f1aa70016d9019c740))
- **selector** Pre-select focused monitor/window and preserve per-mode selection - ([1a1fa86](https://github.com/noirbizarre/snypr/commit/1a1fa86ea5c63fb529ef16a810b2410262949713))
- **selector** Default to Screen (monitor) mode instead of Region - ([28dd523](https://github.com/noirbizarre/snypr/commit/28dd523888c4d44a12bacc0e2ad1086ff9f12795))
- **tray** Host StatusNotifierItem in daemon - ([e31d5b1](https://github.com/noirbizarre/snypr/commit/e31d5b10d9023a04b86ff32d819522ca25087c03))
- **ui** Single toolbar that follows the focused monitor - ([0fc93f8](https://github.com/noirbizarre/snypr/commit/0fc93f8b1fa8f7831acc67d75f5e6ec3e6903223))
- **ui** Bundle 10 more icon-development-kit SVGs and adopt them in toolbars - ([1934deb](https://github.com/noirbizarre/snypr/commit/1934deb3fc28bc689e39a29d9cd9851128918d82))
- **ui** Save in draw overlay via screenshot selector - ([1443396](https://github.com/noirbizarre/snypr/commit/144339621daf5f59f20e9cd83b747860bb8443b9))
- **ui** Per-tool stroke style picker - ([cb9748b](https://github.com/noirbizarre/snypr/commit/cb9748b6372857147ce0de22e74366c68c01178b))
- **ui** Per-tool color picker in editor and draw toolbars - ([3b67048](https://github.com/noirbizarre/snypr/commit/3b67048d8e15269b0facec85dfd50ee9d2bd128c))
- **ui** Shift+click on Capture routes through annotate - ([8832942](https://github.com/noirbizarre/snypr/commit/8832942a55a6f9fa4fa71b28ead78f00ab3d0f5c))
- **ui** Ship app icon and rename app id to noirbizar.re.HyprSnap - ([8e540ef](https://github.com/noirbizarre/snypr/commit/8e540ef643e33c99fe6ad4cf83d50b1fc441be70))
- **ui**  🚨 **breaking** In-place annotation overlay replaces editor window - ([4b07824](https://github.com/noirbizarre/snypr/commit/4b07824510e886d469f40bb68a372ef34d05a350))
- **ui** Unified Toolbar widget shared by selector, editor, and overlay - ([6eeb7bd](https://github.com/noirbizarre/snypr/commit/6eeb7bd7676453e00db37183fd2c4ad2cf1cab98))
- **ui** Live draw-on-screen overlay - ([5beeb72](https://github.com/noirbizarre/snypr/commit/5beeb7206e4fc4697b851642036af1b52f2669d7))
- **ui** Interactive region selector overlay - ([461bce5](https://github.com/noirbizarre/snypr/commit/461bce5c1a2a7e81df9c8631f481311dd132653f))
- Notify on error and plumb -v/-vv into tracing - ([5cb38bc](https://github.com/noirbizarre/snypr/commit/5cb38bc99be9a839972df721dba85201d2ce8407))
- Initial hyprsnap scaffold with wlr-screencopy capture - ([c2575da](https://github.com/noirbizarre/snypr/commit/c2575da2fcd6a951ae1fc611ef2ef46a6b07b338))

### 🐛 Bug Fixes

- **changelog** Keep bot accounts out of New Contributors - ([fe97507](https://github.com/noirbizarre/snypr/commit/fe975078884e3502538314158b0cdf7fd2ce1ac2))
- **cli** Treat selector cancellation as clean exit ([#3](https://github.com/noirbizarre/snypr/issues/3)) - ([a91956a](https://github.com/noirbizarre/snypr/commit/a91956ab4e4056fbb068d662c19a220567a6940d))
- **hypr** Connect directly to the IPC socket, drop hyprland crate - ([8dd1cf8](https://github.com/noirbizarre/snypr/commit/8dd1cf88caabd9353e29aeda72e6f3b31868373c))
- **icons** Vendor the window selector icon - ([743f26b](https://github.com/noirbizarre/snypr/commit/743f26b3bda61e5946b31bbe1b239e5712c011a0))
- **overlay** Keep selector veil visible while annotating - ([1aeb45a](https://github.com/noirbizarre/snypr/commit/1aeb45ad4165e1f463eb688bccedfa92234bb781))
- **selector** Blank overlay surfaces before destroy to defeat fadeOut leak - ([4ddfd6a](https://github.com/noirbizarre/snypr/commit/4ddfd6ab3339bb94bc7d61ee5b9ad68dc6d285e9))
- **selector** Suppress Shift→Annotate when popped from draw overlay - ([bc08099](https://github.com/noirbizarre/snypr/commit/bc080997f25b07bd571c897f40a6eaf8a1205580))
- **selector** Unify validation flow across all selection modes - ([f3b6b35](https://github.com/noirbizarre/snypr/commit/f3b6b35783c2bcc5a114a9eba11357ac2cec1b6b))
- **ui** Use scalable/actions/ layout for bundled symbolic icons - ([c38649e](https://github.com/noirbizarre/snypr/commit/c38649e07e2f3c698a680fe3badeb4fbc09fa6bf))
- **ui** Make stroke-style picker toggles square - ([1897152](https://github.com/noirbizarre/snypr/commit/18971520557c6ae46bfe714bde1daf88268215d1))
- **ui** Inline stroke-style picker instead of using popover - ([8388d4e](https://github.com/noirbizarre/snypr/commit/8388d4eeb14753d3a20e854ae0412e548f9e9ac9))
- **ui** Persist stroke-style selection across tool switches - ([a65a613](https://github.com/noirbizarre/snypr/commit/a65a6135557ce4e7bfd2d091dff38c298b94240f))
- **ui** Make draw overlay passthrough actually pass clicks through - ([3045386](https://github.com/noirbizarre/snypr/commit/3045386b9f77a463deaf6cf8b7af9a716c37f7e3))
- **ui** Selector veil sizing, shared state, and synchronous teardown - ([d8376e2](https://github.com/noirbizarre/snypr/commit/d8376e2fb68343831880d534f274ccfdf32dee05))

### ⚡ Performance

- **ui** Present multi-monitor overlays in a single batch - ([23ac2ae](https://github.com/noirbizarre/snypr/commit/23ac2ae66bd52f73cc0fc1476dc55e7635914323))

### 🔨 Refactor

- **cli**  🚨 **breaking** Remove `annotate` subcommand; editor lives only behind `screenshot --edit` - ([24dcb70](https://github.com/noirbizarre/snypr/commit/24dcb70ad8ef7cff3211b89bd95dab1e68e20d0b))
- **notify** Reuse ui::APP_ID for the notification icon - ([2971deb](https://github.com/noirbizarre/snypr/commit/2971debf9730a8e948018d4e3a7950d5b15c7ff6))
- **overlay** Bail out with `?` instead of a match - ([57bbf78](https://github.com/noirbizarre/snypr/commit/57bbf78f34c302691e3a15ace300419c31e972f5))
- **ui** Split selector into embeddable + standalone entry points - ([ba2c971](https://github.com/noirbizarre/snypr/commit/ba2c9712ff679302cca1b3c7c3ed8094bc694929))
- **ui** Finish Cairo to GSK migration - ([e3f4ade](https://github.com/noirbizarre/snypr/commit/e3f4ade7ca89e93908deca9c5823283a039aa03f))
- **ui/canvas** Render via GSK render nodes - ([4c6820a](https://github.com/noirbizarre/snypr/commit/4c6820a43ddbf5d32341b9643bf4a680c966ce41))
-  🚨 **breaking** Rename hyprsnap to snypr - ([aa0765a](https://github.com/noirbizarre/snypr/commit/aa0765a90a1610d2d82ad5510377afd8621c2678))

### 📚 Documentation

- **readme** Add logo to the header - ([86d383b](https://github.com/noirbizarre/snypr/commit/86d383beccc6cf75a85a749ad2e1ed8b9498617f))
- Fix consistency issues across docs and source comments - ([851272e](https://github.com/noirbizarre/snypr/commit/851272eb74a664786c971e474ce4ae510d180f39))
- List build and runtime system dependencies - ([2fa43fd](https://github.com/noirbizarre/snypr/commit/2fa43fd986e701a9f83e31d805c4f206c9e5bfa8))
- Migrate Hyprland keybindings to Lua syntax - ([09910a3](https://github.com/noirbizarre/snypr/commit/09910a3c0a7f24ce42c95bd8cf7bbb587697dc91))
- Document icon-development-kit attribution and bundling workflow - ([cf9293f](https://github.com/noirbizarre/snypr/commit/cf9293f126ac4c5241e525d0c107dd9451cfa43b))
- Refresh README, add hyprland sample and man page - ([b081339](https://github.com/noirbizarre/snypr/commit/b081339347ab39688d331aff82cb8b37435b519c))

### 🏗️ Build

- **artwork** Generate icons and the social preview from the SVGs - ([961d8b4](https://github.com/noirbizarre/snypr/commit/961d8b41ae01e7a2616005f0454e7310c2ea5068))
- **deps** Bump smithay-client-toolkit from 0.20.0 to 0.21.1 ([#7](https://github.com/noirbizarre/snypr/issues/7)) - ([6595f50](https://github.com/noirbizarre/snypr/commit/6595f507d349fc54fd0347f8ef59107a6eaa06b9))
- **deps** Bump i18n-embed-fl from 0.9.4 to 0.10.1 ([#5](https://github.com/noirbizarre/snypr/issues/5)) - ([589c3e1](https://github.com/noirbizarre/snypr/commit/589c3e187b63161b4275a8f03ed29ee667985ed6))
- **deps** Bump i18n-embed from 0.15.4 to 0.16.0 ([#4](https://github.com/noirbizarre/snypr/issues/4)) - ([47c1760](https://github.com/noirbizarre/snypr/commit/47c176083a22dcf1d7529936f3655d7d969dee78))
- **deps** Bump fluent from 0.16.1 to 0.17.0 ([#6](https://github.com/noirbizarre/snypr/issues/6)) - ([74729d1](https://github.com/noirbizarre/snypr/commit/74729d15167aa90d0a5e5920fc4d71de75aeb718))
- **deps** Update toml requirement from 0.8 to 1.1 ([#1](https://github.com/noirbizarre/snypr/issues/1)) - ([80d9dad](https://github.com/noirbizarre/snypr/commit/80d9dadffb12f95af633d6550bc400e8bbd58eed))
- **deps** Update rstest requirement from 0.23 to 0.26 ([#2](https://github.com/noirbizarre/snypr/issues/2)) - ([156b7b8](https://github.com/noirbizarre/snypr/commit/156b7b833ff2ccce466fb5622d38cf9ef32015db))
- Bundle icons via gresource and register resource path on GTK startup - ([06e8562](https://github.com/noirbizarre/snypr/commit/06e8562169834b6f4cf0ab71ab53edd63e987ff8))

### 🔧 CI

- **aur** Publish from a dedicated aur environment - ([6c35b79](https://github.com/noirbizarre/snypr/commit/6c35b79d8ca57a2e697bac52d4a63756ac475490))
- **mise** Lock tool URLs so locked installs resolve - ([e568e69](https://github.com/noirbizarre/snypr/commit/e568e694504361260c59687b16b53976b0463970))
- **release** Use a conventional Release PR title - ([1fc1906](https://github.com/noirbizarre/snypr/commit/1fc190609c1e8864c0d3b7d396238f0f3002df46))
- **release** Orchestrate releases with gh-ship and git-cliff - ([19ca4d2](https://github.com/noirbizarre/snypr/commit/19ca4d2577386a728908ddcbe1d8f0fb07465eac))
- Disable vapi in gtk4-layer-shell build - ([d648c9a](https://github.com/noirbizarre/snypr/commit/d648c9ac43db26d6c3d93fbde6d1f9ee02ad03a3))
- Build gtk4-layer-shell from source - ([d4c0ae6](https://github.com/noirbizarre/snypr/commit/d4c0ae610aaaccb7fa8f7219ab3b6303630b8142))

### 🧹 Chores

- **artwork** Clean the SVG sources - ([81eab20](https://github.com/noirbizarre/snypr/commit/81eab20f6efd50a7cdde97f07665448631a224af))
- **ui** Remove unused placeholder_path stub - ([8422684](https://github.com/noirbizarre/snypr/commit/8422684048c838c70457c21f87f38f593a14a1c4))
- Reformat Cargo.toml with taplo - ([ff9a404](https://github.com/noirbizarre/snypr/commit/ff9a4048bbdbd92221361fc05f3b916213cba27f))

## ❤️ New Contributors

* @noirbizarre made their first contribution
