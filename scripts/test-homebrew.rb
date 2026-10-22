require "json"
require "open3"
require "tmpdir"
require "formulary"
require "extend/ENV"

ENV.activate_extensions!(env: "std")

def check(condition, message)
  raise message unless condition
end

generator = File.expand_path("homebrew-formula.rb", __dir__)
Dir.mktmpdir("kodrik-homebrew-") do |directory|
  archive = File.join(directory, "kodrik-1.2.3-aarch64-apple-darwin.tar.gz")
  File.write(archive, "abc")
  output, error, status = Open3.capture3(RbConfig.ruby, generator, "v1.2.3", archive)
  check(status.success?, "Formula generation failed: #{error}")
  path = Pathname.new(directory)/"kodrik.rb"
  formula = Formulary.from_contents("kodrik", path, output)
  check(formula.version.to_s == "1.2.3", "Wrong formula version")
  check(formula.stable.url == "https://github.com/suinly/kodrik/releases/download/v1.2.3/kodrik-1.2.3-aarch64-apple-darwin.tar.gz", "Wrong release URL")
  check(formula.stable.checksum.to_s == "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad", "Checksum does not match archive bytes")
  check(formula.deps.empty?, "Stable binary installation must not need build tools")

  head = Formulary.from_contents("kodrik", path, output, :head)
  head.build = BuildOptions.new(Options.create(["--HEAD"]), head.options)
  check(head.head&.url == "https://github.com/suinly/kodrik.git", "HEAD must build the upstream repository")
  check(head.head.specs[:branch] == "main", "HEAD must follow main")
  check(head.deps.map(&:name).sort == ["node@24", "rust"], "HEAD needs Node.js and Rust")
  check(head.deps.all?(&:build?), "Build tools must not become runtime dependencies")

  commands = []
  head.define_singleton_method(:system) { |*arguments| commands << arguments.map(&:to_s) }
  head.fetch
  head.install
  check(commands[0].first(5) == ["npm", "ci", "--prefix", "web", "--include=dev"], "Fetch must install frontend build dependencies")
  check(!commands[0].include?("--ignore-scripts"), "Frontend security patches require npm lifecycle scripts")
  check(commands[1].first(2) == ["cargo", "fetch"] && commands[1].include?("--locked"), "Fetch must honor Cargo.lock")
  check(commands[2].first(5) == ["npm", "run", "build", "--prefix", "web"], "Build embedded frontend before Rust")
  check(commands[3].first(2) == ["cargo", "install"] && commands[3].include?("--locked") && commands[3].include?("--offline"), "Install must build Rust offline with the lockfile")
  check(commands[4] == ["/usr/bin/codesign", "--force", "--sign", "-", "--identifier", "com.kodrik.gateway", "--timestamp=none", "#{head.bin}/kodrik"], "HEAD executable must keep the gateway signing identifier")
  check(commands[5] == ["/usr/bin/codesign", "--verify", "--strict", "#{head.bin}/kodrik"], "Verify HEAD signature before installation completes")
  check(commands.length == 6, "Unexpected HEAD build steps")

  commands.clear
  installed = []
  destination = Object.new
  destination.define_singleton_method(:install) { |source| installed << source }
  formula.define_singleton_method(:bin) { destination }
  formula.define_singleton_method(:system) { |*arguments| commands << arguments }
  formula.fetch
  formula.install
  check(commands.empty? && installed == ["kodrik"], "Stable installation must use the packaged executable")

  plist, error, status = Open3.capture3("/usr/bin/plutil", "-convert", "json", "-o", "-", "-", stdin_data: formula.service.to_plist)
  check(status.success?, "Invalid service plist: #{error}")
  service = JSON.parse(plist)
  check(service.fetch("Label") == "com.kodrik.gateway", "Service migration changed the launchd label")
  check(service.fetch("ProgramArguments") == ["#{formula.opt_bin}/kodrik", "gateway"], "Service does not use the stable executable path")
  check(service.fetch("KeepAlive") == true, "Cooperative restart needs KeepAlive")
  check(service.fetch("WorkingDirectory") == "#{Dir.home}/.kodrik/workspace", "Service must not work in the Cellar or source checkout")
  check(service.fetch("StandardOutPath") == "#{Dir.home}/.kodrik/logs/kodrik-gateway.log", "Wrong log destination")
  check(service.fetch("EnvironmentVariables").fetch("PATH").split(":").include?("#{HOMEBREW_PREFIX}/bin"), "Homebrew tools are absent from service PATH")

  [[], ["1.2.3", archive], ["v1.2.3-rc.1", archive], ["v1.2.3\nputs :injected", archive], ["v1.2.3", "#{directory}/missing.tar.gz"]].each do |arguments|
    output, _, status = Open3.capture3(RbConfig.ruby, generator, *arguments)
    check(!status.success? && output.empty?, "Invalid input generated a formula: #{arguments.inspect}")
  end
end
puts "Homebrew formula and service checks passed."
