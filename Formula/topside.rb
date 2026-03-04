class Topside < Formula
  desc "Agent-native local project management and knowledge hub"
  homepage "https://github.com/anthonymarti/topside"
  url "https://github.com/anthonymarti/topside/archive/refs/tags/v0.1.0.tar.gz"
  sha256 "REPLACE_WITH_RELEASE_TARBALL_SHA256"
  license "MIT OR Apache-2.0"

  depends_on "rust" => :build

  def install
    system "cargo", "install", *std_cargo_args(path: ".")
  end

  test do
    system "#{bin}/topside", "--version"
  end
end
