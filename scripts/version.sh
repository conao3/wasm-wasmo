#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."
version="$(tr -d '[:space:]' < VERSION)"
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]] || { echo "VERSION is not semver: $version" >&2; exit 1; }

current() {
  echo "Cargo.toml $(sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\(.*\)"/\1/p' Cargo.toml)"
  echo "Cargo.lock $(sed -n '/^name = "wasmo-core"$/{n;s/^version = "\(.*\)"/\1/p}' Cargo.lock)"
  echo "Cargo.lock $(sed -n '/^name = "wasmo-wasm"$/{n;s/^version = "\(.*\)"/\1/p}' Cargo.lock)"
  echo "js/package.json $(jq -r .version js/package.json)"
  echo "js/package-lock.json $(jq -r .version js/package-lock.json)"
  echo "js/package-lock.json $(jq -r '.packages[""].version' js/package-lock.json)"
  echo "jvm/pom.xml $(sed -n 's|^  <version>\(.*\)</version>$|\1|p' jvm/pom.xml)"
  echo "dotnet/src/Wasmo/Wasmo.csproj $(sed -n 's|^ *<Version>\(.*\)</Version>$|\1|p' dotnet/src/Wasmo/Wasmo.csproj)"
  echo "python/pyproject.toml $(sed -n 's/^version = "\(.*\)"$/\1/p' python/pyproject.toml)"
  echo "ruby/wasmo.gemspec $(sed -n 's/^  spec.version = "\(.*\)"$/\1/p' ruby/wasmo.gemspec)"
  echo "ruby/Gemfile.lock $(sed -n 's/^    wasmo (\(.*\))$/\1/p' ruby/Gemfile.lock)"
}

case "${1:-}" in
  get)
    echo "$version"
    ;;
  set)
    sed -i '/^\[workspace.package\]/,/^\[/s/^version = ".*"/version = "'"$version"'"/' Cargo.toml
    sed -i '/^name = "wasmo-\(core\|wasm\)"$/{n;s/^version = ".*"/version = "'"$version"'"/}' Cargo.lock
    (cd js && npm version "$version" --no-git-tag-version --allow-same-version > /dev/null)
    sed -i 's|^  <version>.*</version>$|  <version>'"$version"'</version>|' jvm/pom.xml
    sed -i 's|^\( *\)<Version>.*</Version>$|\1<Version>'"$version"'</Version>|' dotnet/src/Wasmo/Wasmo.csproj
    sed -i 's/^version = ".*"$/version = "'"$version"'"/' python/pyproject.toml
    sed -i 's/^  spec.version = ".*"$/  spec.version = "'"$version"'"/' ruby/wasmo.gemspec
    sed -i 's/^    wasmo (.*)$/    wasmo ('"$version"')/' ruby/Gemfile.lock
    exec scripts/version.sh check
    ;;
  check)
    status=0
    while read -r file found; do
      if [[ "$found" != "$version" ]]; then
        echo "$file has version '$found', VERSION is $version" >&2
        status=1
      fi
    done < <(current)
    if [[ -n "${2:-}" && "$2" != "v$version" ]]; then
      echo "tag $2 does not match VERSION $version" >&2
      status=1
    fi
    exit "$status"
    ;;
  *)
    echo "usage: $0 get | set | check [tag]" >&2
    exit 2
    ;;
esac
