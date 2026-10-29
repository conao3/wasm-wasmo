# Releasing

Every package ships the same version from `VERSION` and the same `wasmo.wasm`. A `v<VERSION>` tag starts `.github/workflows/release.yml`, which builds the module once, tests and packs every adapter, publishes each registry from its own job, tags the Go module, and creates the GitHub Release.

| Package | Registry | Authentication | GitHub environment |
| --- | --- | --- | --- |
| `@conao3/wasmo` | npm | Trusted publishing (OIDC), provenance | `npm` |
| `wasmo` | PyPI | Trusted publishing (OIDC), attestations | `pypi` |
| `wasmo` | RubyGems | Trusted publishing (OIDC) | `rubygems` |
| `Wasmo` | NuGet | Trusted publishing (`NuGet/login`) | `nuget` |
| `io.github.conao3:wasmo` | Maven Central (Central Portal) | User token, GPG signature | `maven-central` |
| `github.com/conao3/wasm-wasmo/go` | Go module proxy | `go/v<VERSION>` tag pushed with `GITHUB_TOKEN` | none |

## How the Go module gets `wasmo.wasm`

`go/wasmo.wasm` is a build output and stays out of `main`. For each release, `scripts/go-release.sh` makes one commit on top of the tagged commit that adds the built `go/wasmo.wasm` (and drops its line from `.gitignore`), and tags that commit `go/v<VERSION>`. The commit is reachable only from the tag. `go get github.com/conao3/wasm-wasmo/go@v<VERSION>` resolves the `go/v<VERSION>` tag, so the module zip on the proxy contains the same `wasmo.wasm` as every other package, and `go:embed` needs no generate step on the consumer side.

Before pushing, the script serves the module zip from a local `GOPROXY=file://` directory and builds and runs a consumer program against it. `make dist-go` runs the same check without tagging. Only tagged versions are installable; a pseudo-version of `main` has no `wasmo.wasm`. Go tags are permanent once the proxy has fetched them, so a broken Go release is fixed with a new version, never by moving a tag.

## One-time setup

Do these once, before the first tag. Right before the first release, check that the names are still free: each of `curl -s -o /dev/null -w "%{http_code}"` on `https://registry.npmjs.org/@conao3%2fwasmo`, `https://pypi.org/pypi/wasmo/json`, `https://rubygems.org/api/v1/gems/wasmo.json`, and `https://api.nuget.org/v3-flatcontainer/wasmo/index.json` returns `404`.

### GitHub

1. Settings > Environments: create `npm`, `pypi`, `rubygems`, `nuget`, and `maven-central`. Add yourself as a required reviewer on each so every publish waits for an approval, and limit deployment to tags matching `v*`.
2. Settings > Rules > Rulesets: add a tag ruleset for `v*` and `go/v*` that blocks deletion and non-fast-forward updates. Allow GitHub Actions to create tags (the `go` job pushes `go/v*`).
3. Settings > Actions > General: keep "Read repository contents" as the default token permission. The `go` and `github-release` jobs request `contents: write` themselves.

### npm (`@conao3/wasmo`)

npm only offers trusted publishing for a package that already exists, so the first release uses a token.

1. On npmjs.com, as `conao3`, enable 2FA. Create a granular access token with read and write access to packages and the "bypass 2FA" option, expiring in 7 days.
2. Store it as the secret `NPM_TOKEN` in the `npm` environment.
3. After `0.1.0` is on npm: package settings > Trusted Publisher > GitHub Actions, owner `conao3`, repository `wasm-wasmo`, workflow `release.yml`, environment `npm`, and allow `npm publish`. A new trusted publisher must complete its first publish within 2 days, so add it shortly before the next release.
4. Delete the `NPM_TOKEN` secret, revoke the token, and set Publishing access to "Require two-factor authentication and disallow tokens".

The npm CLI tries OIDC first and uses `NODE_AUTH_TOKEN` only when OIDC is not accepted, so the workflow needs no change between the two phases.

### PyPI (`wasmo`)

1. On pypi.org, as `conao3`: Your account > Publishing > Add a new pending publisher > GitHub.
2. PyPI project name `wasmo`, owner `conao3`, repository `wasm-wasmo`, workflow `release.yml`, environment `pypi`.

The pending publisher turns into a normal trusted publisher when the first upload creates the project. No secret is needed.

### RubyGems (`wasmo`)

1. On rubygems.org, as `conao3`, enable MFA (the gemspec sets `rubygems_mfa_required`).
2. Settings > Trusted publishers > Create a pending trusted publisher: gem name `wasmo`, GitHub Actions, repository owner `conao3`, repository `wasm-wasmo`, workflow `release.yml`, environment `rubygems`.

No secret is needed.

### NuGet (`Wasmo`)

1. On nuget.org, sign in as `conao3` and open Trusted Publishing under your account. Add a policy: repository owner `conao3`, repository `wasm-wasmo`, workflow file `release.yml`, environment `nuget`.
2. Store your nuget.org user name (the profile name, not the email address) as the secret `NUGET_USER` in the `nuget` environment.
3. Optional: reserve the `Wasmo` ID prefix by mailing account@nuget.org from the account's address with the prefix and the owner `conao3`. Reservation adds the verified checkmark and stops others from publishing `Wasmo.*`.

### Maven Central (`io.github.conao3:wasmo`)

1. Sign in to <https://central.sonatype.com> with the GitHub account `conao3`. Signing in with GitHub verifies the namespace `io.github.conao3`; check it under Namespaces. If you signed up another way, add the namespace, create the public repository named by the verification key under `github.com/conao3`, press Verify, and delete the repository afterwards.
2. Account > Generate User Token. Store the two values as the secrets `MAVEN_CENTRAL_USERNAME` and `MAVEN_CENTRAL_PASSWORD` in the `maven-central` environment.
3. Create a signing key: `gpg --quick-gen-key "conao3 <address>" ed25519 sign 3y`. Publish the public key: `gpg --keyserver keyserver.ubuntu.com --send-keys <fingerprint>` and upload it at <https://keys.openpgp.org/upload> (verify the address in the mail it sends).
4. Store `gpg --armor --export-secret-keys <fingerprint>` as the secret `MAVEN_GPG_KEY` and its passphrase as `MAVEN_GPG_PASSPHRASE` in the `maven-central` environment. `maven-gpg-plugin` signs with Bouncy Castle from these variables, so the runner needs no `gpg`.

### Go

Nothing to register. pkg.go.dev lists the module after the proxy fetches it; the `go` job asks the proxy for the new version right after pushing the tag.

## Release checklist

1. Start from an up-to-date `main` with a green `ci` run.
2. Write the new version into `VERSION` and run `nix develop --command make version`. It rewrites `Cargo.toml`, `Cargo.lock`, `js/package.json`, `js/package-lock.json`, `jvm/pom.xml`, `dotnet/src/Wasmo/Wasmo.csproj`, `python/pyproject.toml`, `ruby/wasmo.gemspec`, and `ruby/Gemfile.lock`. `make lint` fails while any of them disagrees with `VERSION`.
3. In `CHANGELOG.md`, turn the notes into `## [<VERSION>] - <YYYY-MM-DD>` and add the link reference at the bottom. The GitHub Release body is this section.
4. Commit (`release: v<VERSION>`), open a pull request, and merge it after `ci` passes.
5. Dry run: Actions > release > Run workflow on `main` with "dry-run" checked. The `build` job runs `make test` and `make dist` (npm tarball, jar with sources and javadoc jars, nupkg and snupkg, sdist and wheel, gem, and the Go module zip checked by a consumer build) and uploads `dist/` as the `dist` artifact. Every publish job is skipped. Download the artifact and look inside the packages if anything packaging-related changed.
6. Tag the merge commit and push the tag:

   ```sh
   git switch main && git pull
   git tag -a "v$(cat VERSION)" -m "v$(cat VERSION)"
   git push origin "v$(cat VERSION)"
   ```

7. Approve the five environments when GitHub asks. Each publish job first checks that the tag equals `v` + `VERSION` and that every manifest agrees, and stops otherwise. The `go` job runs only after all five registries succeed, and the GitHub Release (with `wasmo.wasm`, every package, and `SHA256SUMS`) is created last.
8. Check every registry:

   ```sh
   npm view @conao3/wasmo@<VERSION> dist.attestations
   pip download --no-deps wasmo==<VERSION>
   gem fetch wasmo -v <VERSION>
   curl -sI https://api.nuget.org/v3-flatcontainer/wasmo/<VERSION>/wasmo.<VERSION>.nupkg
   curl -sI https://repo1.maven.org/maven2/io/github/conao3/wasmo/<VERSION>/wasmo-<VERSION>.jar
   GOPROXY=https://proxy.golang.org go list -m github.com/conao3/wasm-wasmo/go@v<VERSION>
   ```

   Compare the downloaded files with `SHA256SUMS` on the GitHub Release. Maven Central and the NuGet search index can take up to an hour to show a new version.

## When a publish job fails

Registries refuse to overwrite a version, so rerun only the failed job ("Re-run failed jobs"); the jobs that succeeded stay published. The `go` and `github-release` jobs wait for all five registries, so they run once the rerun succeeds. If a package is broken after it was published, release the next patch version; npm, PyPI, RubyGems, and NuGet allow deprecating or yanking the broken one, and Maven Central does not allow removal.
