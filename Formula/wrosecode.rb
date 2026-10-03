class Wrosecode < Formula
  desc "Terminal native CTF and coding agent"
  homepage "https://github.com/example/wrosecode"
  url "https://github.com/example/wrosecode/archive/refs/tags/v0.2.0.tar.gz"
  version "0.2.0"
  license "MIT"
  depends_on "rust" => :build
  def install
    system "cargo", "install", *std_cargo_args
  end
  test do
    assert_match "wrosecode", shell_output("#{bin}/wrosecode --version")
  end
end
