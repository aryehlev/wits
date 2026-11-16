# Publishing wits

Everything here is done once, for the one release.

1. **crates.io.** `cargo publish` (or add a `CARGO_REGISTRY_TOKEN` repository
   secret and the release workflow does it). This also reserves the name.
2. **GitHub release.** `git tag v1.0.0 && git push origin v1.0.0`. The
   `release` workflow builds static binaries for x86_64 and aarch64, runs the
   cases on both, and attaches the tarballs, checksums, a filled-in `wits.rb`
   and a filled-in `PKGBUILD`. After this `install.sh` and `cargo binstall wits`
   work.
3. **Homebrew.** Create the repository `aryehlev/homebrew-tap`, download
   `wits.rb` from the release and commit it as `Formula/wits.rb`.
   Users: `brew install aryehlev/tap/wits`.
4. **AUR.** Clone `ssh://aur@aur.archlinux.org/wits-bin.git`, add the released
   `PKGBUILD`, run `makepkg --printsrcinfo > .SRCINFO`, commit both, push.
5. **Demo GIF.** `cargo build --release && vhs demo/demo.tape` writes
   `demo/demo.gif`, which the README shows.
