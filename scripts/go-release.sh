#!/usr/bin/env bash
set -euo pipefail

usage() {
  echo "usage: $0 <wasmo.wasm> [--push]" >&2
  exit 2
}

[[ $# -ge 1 && $# -le 2 ]] || usage
wasm="$(realpath "$1")"
push="${2:-}"
[[ -z "$push" || "$push" == "--push" ]] || usage

root="$(cd "$(dirname "$0")/.." && pwd)"
version="$("$root/scripts/version.sh" get)"
module="github.com/conao3/wasm-wasmo/go"
tag="go/v$version"
out="$root/dist/go"

work="$(mktemp -d)"
cleanup() {
  git -C "$root" worktree remove --force "$work/tree" 2> /dev/null || true
  chmod -R u+w "$work" 2> /dev/null || true
  rm -rf "$work"
}
trap cleanup EXIT

git -C "$root" worktree add --quiet --detach "$work/tree" HEAD
tree="$work/tree"
cp "$wasm" "$tree/go/wasmo.wasm"
sed -i '\|^/go/wasmo.wasm$|d' "$tree/.gitignore"
git -C "$tree" add .gitignore go/wasmo.wasm
git -C "$tree" commit --quiet --no-verify -m "release: $tag with the built wasmo.wasm"
commit="$(git -C "$tree" rev-parse HEAD)"

proxy="$work/proxy/$module/@v"
mkdir -p "$proxy" "$out"
git -C "$tree" archive --format=tar --prefix="$module@v$version/" "$commit:go" | tar -x -C "$work"
(cd "$work" && find "$module@v$version" -type f | LC_ALL=C sort | zip -q -X "$proxy/v$version.zip" -@)
cp "$tree/go/go.mod" "$proxy/v$version.mod"
printf '{"Version":"v%s","Time":"%s"}\n' "$version" "$(git -C "$tree" show -s --format=%cI "$commit")" > "$proxy/v$version.info"
echo "v$version" > "$proxy/list"
unzip -l "$proxy/v$version.zip" | grep -q "$module@v$version/wasmo.wasm"
cp "$proxy/v$version.zip" "$out/wasmo-go-v$version.zip"

consumer="$work/consumer"
mkdir -p "$consumer"
cat > "$consumer/main.go" << 'EOF'
package main

import (
	"context"
	"fmt"

	"github.com/aws/aws-sdk-go-v2/service/sts"
	"github.com/conao3/wasm-wasmo/go"
)

func main() {
	ctx := context.Background()
	m, err := wasmo.New(ctx)
	if err != nil {
		panic(err)
	}
	defer m.Close(ctx)
	out, err := sts.NewFromConfig(m.Config()).GetCallerIdentity(ctx, &sts.GetCallerIdentityInput{})
	if err != nil {
		panic(err)
	}
	fmt.Println(*out.Account)
}
EOF
(
  cd "$consumer"
  export GOFLAGS=-mod=mod GONOSUMDB="$module" GOPROXY="file://$work/proxy,https://proxy.golang.org" GOMODCACHE="$work/modcache" CGO_ENABLED=0
  go mod init example.com/consumer 2> /dev/null
  go get "$module@v$version" github.com/aws/aws-sdk-go-v2/service/sts
  account="$(go run .)"
  [[ "$account" == "000000000000" ]] || { echo "consumer got account '$account'" >&2; exit 1; }
)
echo "go module $module@v$version verified from commit $commit"

if [[ "$push" == "--push" ]]; then
  git -C "$root" tag "$tag" "$commit"
  git -C "$root" push origin "refs/tags/$tag"
fi
