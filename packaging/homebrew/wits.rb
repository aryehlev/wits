# Template: the release workflow fills in the @...@ fields and attaches the
# result to the GitHub release as wits.rb. Copy that file to Formula/wits.rb
# in the tap repository (aryehlev/homebrew-tap).
class Wits < Formula
  desc "Explain why a process is slow, and where in its code"
  homepage "https://github.com/aryehlev/wits"
  version "@VERSION@"
  license "MIT"

  on_macos do
    on_intel do
      url "https://github.com/aryehlev/wits/releases/download/v@VERSION@/wits-x86_64-apple-darwin.tar.gz"
      sha256 "@SHA_MAC_X86_64@"
    end
    on_arm do
      url "https://github.com/aryehlev/wits/releases/download/v@VERSION@/wits-aarch64-apple-darwin.tar.gz"
      sha256 "@SHA_MAC_AARCH64@"
    end
  end

  on_linux do
    on_intel do
      url "https://github.com/aryehlev/wits/releases/download/v@VERSION@/wits-x86_64-unknown-linux-musl.tar.gz"
      sha256 "@SHA_X86_64@"
    end
    on_arm do
      url "https://github.com/aryehlev/wits/releases/download/v@VERSION@/wits-aarch64-unknown-linux-musl.tar.gz"
      sha256 "@SHA_AARCH64@"
    end
  end

  def install
    bin.install "wits"
  end

  test do
    assert_match "wits #{version}", shell_output("#{bin}/wits --version")
  end
end
