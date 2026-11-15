class Ctrlz < Formula
  desc "Undo what a coding agent did to your working tree"
  homepage "https://github.com/akshaytheflash/ctrlz"
  version "{{VERSION}}"
  license "MIT"

  on_macos do
    on_intel do
      url "https://github.com/akshaytheflash/ctrlz/releases/download/v#{version}/ctrlz-x86_64-apple-darwin.tar.gz"
      sha256 "{{SHA_MAC_X64}}"
    end

    on_arm do
      url "https://github.com/akshaytheflash/ctrlz/releases/download/v#{version}/ctrlz-aarch64-apple-darwin.tar.gz"
      sha256 "{{SHA_MAC_ARM64}}"
    end
  end

  on_linux do
    on_intel do
      url "https://github.com/akshaytheflash/ctrlz/releases/download/v#{version}/ctrlz-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "{{SHA_LINUX_X64}}"
    end
  end

  def install
    bin.install "ctrlz"
  end

  test do
    system "#{bin}/ctrlz", "--version"
  end
end
