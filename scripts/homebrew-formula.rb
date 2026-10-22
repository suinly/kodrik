require "digest"

tag, archive = ARGV
abort "Usage: ruby scripts/homebrew-formula.rb vX.Y.Z ARCHIVE" unless ARGV.length == 2 && /\Av\d+\.\d+\.\d+\z/.match?(tag)
abort "Archive does not exist: #{archive}" unless File.file?(archive)

version = tag.delete_prefix("v")
checksum = Digest::SHA256.file(archive).hexdigest
puts <<~FORMULA
  class Kodrik < Formula
    desc "Personal AI companion for terminal, Telegram and browser"
    homepage "https://github.com/suinly/kodrik"
    url "https://github.com/suinly/kodrik/releases/download/#{tag}/kodrik-#{version}-aarch64-apple-darwin.tar.gz"
    version "#{version}"
    sha256 "#{checksum}"

    head do
      url "https://github.com/suinly/kodrik.git", branch: "main"

      depends_on "node@24" => :build
      depends_on "rust" => :build
    end

    depends_on arch: :arm64
    depends_on macos: :sonoma

    def fetch
      return unless build.head?

      system "npm", "ci", "--prefix", "web", "--include=dev", *std_npm_args(prefix: nil, ignore_scripts: false)
      system "cargo", "fetch", *std_cargo_fetch_args
    end

    def install
      if build.head?
        system "npm", "run", "build", "--prefix", "web"
        system "cargo", "install", *std_cargo_args
        system "/usr/bin/codesign", "--force", "--sign", "-", "--identifier", "com.kodrik.gateway",
               "--timestamp=none", bin/"kodrik"
        system "/usr/bin/codesign", "--verify", "--strict", bin/"kodrik"
      else
        bin.install "kodrik"
      end
    end

    def caveats
      <<~EOS
        Create ~/.kodrik/config.toml before starting Kodrik.
        Configuration: https://github.com/suinly/kodrik/blob/main/docs/configuration.md

        Run `kodrik` for the terminal UI. After configuring Telegram and/or web,
        run `brew services start suinly/tap/kodrik` for the background gateway.
        Its working directory is ~/.kodrik/workspace; logs are in ~/.kodrik/logs.
        Stop the source-installed com.kodrik.gateway LaunchAgent before migrating.

        This release is ad-hoc signed, without Apple notarization.
        Uninstalling preserves ~/.kodrik, including credentials and history.
      EOS
    end

    service do
      name macos: "com.kodrik.gateway"
      run [opt_bin/"kodrik", "gateway"]
      keep_alive true
      throttle_interval 30
      working_dir File.join(Dir.home, ".kodrik/workspace")
      log_path File.join(Dir.home, ".kodrik/logs/kodrik-gateway.log")
      error_log_path File.join(Dir.home, ".kodrik/logs/kodrik-gateway.err.log")
      environment_variables PATH:     "\#{Dir.home}/.local/bin:\#{Dir.home}/.cargo/bin:\#{std_service_path_env}",
                            RUST_LOG: "info"
    end

    test do
      expected_version = build.head? ? "kodrik " : "kodrik #{version}"
      assert_match expected_version, shell_output("\#{bin}/kodrik --version")
      result = JSON.parse(shell_output("\#{bin}/kodrik invites list"))
      assert_empty result.fetch("invites")
      assert_empty result.fetch("users")
    end
  end
FORMULA
