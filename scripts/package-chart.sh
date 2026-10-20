#!/usr/bin/env bash
# Set image defaults in the published artifact without changing the source chart.
set -euo pipefail

chart="${1:?chart directory is required}"
export IMAGE_REPOSITORY="${2:?image repository is required}"
export IMAGE_TAG="${3-}"
destination="${4:?package destination is required}"
package_dir="$(mktemp -d)"
trap 'rm -rf "$package_dir"' EXIT
cp -a "$chart" "$package_dir/chart"

env -u GEM_HOME -u GEM_PATH -u RUBYLIB -u RUBYOPT ruby -ryaml - "$package_dir/chart/values.yaml" <<'RUBY'
path = ARGV.fetch(0)
values = YAML.load_file(path)
image = values.fetch('image')
image['repository'] = ENV.fetch('IMAGE_REPOSITORY').downcase
tag = ENV.fetch('IMAGE_TAG')
image['tag'] = tag unless tag.empty?
File.write(path, YAML.dump(values))
RUBY

helm package "$package_dir/chart" --destination "$destination"
