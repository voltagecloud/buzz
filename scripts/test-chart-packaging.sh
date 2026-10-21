#!/usr/bin/env bash
set -euo pipefail
chart="${1:?chart directory is required}"
test_dir="$(mktemp -d)"
trap 'rm -rf "$test_dir"' EXIT
cp "$chart/values.yaml" "$test_dir/source-values.yaml"

for scenario in upstream fork override; do
  case "$scenario" in
    upstream) repository=ghcr.io/block/buzz; tag='' ;;
    fork) repository=GHCR.IO/VoltageCloud/Buzz; tag=main ;;
    override) repository=GHCR.IO/Custom/Relay; tag=release-123 ;;
  esac
  destination="$test_dir/$scenario"
  mkdir -p "$destination"
  scripts/package-chart.sh "$chart" "$repository" "$tag" "$destination"
  helm show values "$destination"/*.tgz > "$destination/values.yaml"
  env -u GEM_HOME -u GEM_PATH -u RUBYLIB -u RUBYOPT ruby -ryaml - \
    "$test_dir/source-values.yaml" "$destination/values.yaml" "$repository" "$tag" <<'RUBY'
source = YAML.load_file(ARGV.fetch(0))
packaged = YAML.load_file(ARGV.fetch(1))
expected = source.dup
expected['image'] = source.fetch('image').merge('repository' => ARGV.fetch(2).downcase)
expected['image']['tag'] = ARGV.fetch(3) unless ARGV.fetch(3).empty?
raise 'packaged values do not match the selected image' unless packaged == expected
RUBY
done
cmp "$chart/values.yaml" "$test_dir/source-values.yaml"
