# Releasing

Every package ships the same version from `VERSION` and the same `wasmo.wasm`. One manual run of `.github/workflows/release.yml` on `main` bumps the version, builds the module once, packs every adapter, pushes the version commit to `main` together with the annotated tag `v<VERSION>`, publishes each registry from its own job, tags the Go module, and creates the GitHub Release. The workflow pushes the tags itself.

| Package | Registry | Authentication | GitHub environment |
| --- | --- | --- | --- |
| `@conao3/wasmo` | npm | Trusted publishing (OIDC), provenance | `npm` |
| `wasmo` | PyPI | Trusted publishing (OIDC), attestations | `pypi` |
| `wasmo` | RubyGems | Trusted publishing (OIDC) | `rubygems` |
| `Wasmo` | NuGet | Trusted publishing (`NuGet/login`) | `nuget` |
| `io.github.conao3:wasmo` | Maven Central (Central Portal) | User token, GPG signature | `maven-central` |
| `github.com/conao3/wasm-wasmo/go` | Go module proxy | `go/v<VERSION>` tag pushed with `GITHUB_TOKEN` | none |

## How the Go module gets `wasmo.wasm`

`go/wasmo.wasm` is a build output and stays out of `main`. For each release, `scripts/go-release.sh` makes one commit on top of the `v<VERSION>` commit that adds the built `go/wasmo.wasm` (and drops its line from `.gitignore`), and tags that commit `go/v<VERSION>`. The commit is reachable only from the tag. `go get github.com/conao3/wasm-wasmo/go@v<VERSION>` resolves the `go/v<VERSION>` tag, so the module zip on the proxy contains the same `wasmo.wasm` as every other package, and `go:embed` needs no generate step on the consumer side.

Before pushing, the script serves the module zip from a local `GOPROXY=file://` directory and builds and runs a consumer program against it. `make dist-go` runs the same check without tagging. Only tagged versions are installable; a pseudo-version of `main` has no `wasmo.wasm`. Go tags are permanent once the proxy has fetched them, so a broken Go release is fixed with a new version, never by moving a tag.

## One-time setup

Do these once, before the first release. Right before the first release, check that the names are still free: each of `curl -s -o /dev/null -w "%{http_code}"` on `https://registry.npmjs.org/@conao3%2fwasmo`, `https://pypi.org/pypi/wasmo/json`, `https://rubygems.org/api/v1/gems/wasmo.json`, and `https://api.nuget.org/v3-flatcontainer/wasmo/index.json` returns `404`.

### GitHub

1. Settings > Environments: create `npm`, `pypi`, `rubygems`, `nuget`, and `maven-central`. Add yourself as a required reviewer on each so every publish waits for an approval, and limit deployment to the branch `main` (the release workflow runs on `main`).
2. Settings > Rules > Rulesets: add a tag ruleset for `v*` and `go/v*` that blocks deletion and non-fast-forward updates. Allow GitHub Actions to create tags (the `tag` job pushes `v*` and the `go` job pushes `go/v*`).
3. Settings > Actions > General: keep "Read repository contents" as the default token permission. The `tag`, `go`, and `github-release` jobs request `contents: write` themselves.

### npm (`@conao3/wasmo`)

npm only accepts a trusted publisher for a package that already exists, so the name is created once by hand with a placeholder version, and every real version is published from GitHub Actions with OIDC.

1. On npmjs.com, as `conao3`, enable 2FA.
2. Publish the placeholder: in an empty directory, write a `package.json` with name `@conao3/wasmo`, version `0.0.0`, and a README that points to this repository, then run `npm login` and `npm publish --access public`.
3. Register the trusted publisher (npm 11.15.0 or later): `npm trust github @conao3/wasmo --file release.yml --repo conao3/wasm-wasmo --env npm --allow-publish`. A new trusted publisher must complete its first publish within 2 days, so do this shortly before the first release.
4. Package settings > Publishing access: "Require two-factor authentication and disallow tokens".
5. After the first real version is on npm, remove the placeholder with `npm unpublish @conao3/wasmo@0.0.0` (allowed within 72 hours of its publication).

The `npm` job publishes with the OIDC token of the run and needs no secret. npm 11.5.1 or later is required, so the job installs the latest npm first.

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

1. Start from an up-to-date `main` with a green `ci` run. `VERSION` holds the latest published version.
2. Optional dry run: `gh workflow run release.yml --ref main -f version_bump=patch -f dry_run=true`. The `check` job bumps the version in a local commit, runs `make dist` (npm tarball, jar with sources and javadoc jars, nupkg and snupkg, sdist and wheel, gem, and the Go module zip checked by a consumer build), and uploads `dist/` as the `dist` artifact. Nothing is pushed and every publish job is skipped. Download the artifact and look inside the packages if anything packaging-related changed.
3. Release: `gh workflow run release.yml --ref main -f version_bump=patch` (or `minor`, `major`). The `check` job computes the next version from `VERSION`, stops if `v<VERSION>` or `go/v<VERSION>` already exists, writes `VERSION`, runs `make version` (it rewrites `Cargo.toml`, `Cargo.lock`, `js/package.json`, `js/package-lock.json`, `jvm/pom.xml`, `dotnet/src/Wasmo/Wasmo.csproj`, `python/pyproject.toml`, `ruby/wasmo.gemspec`, and `ruby/Gemfile.lock`), commits `release: v<VERSION>`, and runs `make dist`. The `tag` job checks that the commit is a child of the dispatched commit and touches only those files, then pushes it to `main` and the annotated tag `v<VERSION>` in one atomic push.
4. Approve the five environments when GitHub asks. Each publish job checks out the tag, checks that it equals `v` + `VERSION` and that every manifest agrees, and publishes the package from the `dist` artifact (the `maven` job rebuilds the jars and compares them with the artifact before it deploys). The `go` job runs only after all five registries succeed, and the GitHub Release (notes generated by GitHub, with `wasmo.wasm`, every package, and `SHA256SUMS`) is created last.
5. Check every registry:

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

Registries refuse to overwrite a version, so rerun only the failed job in the same run ("Re-run failed jobs", `gh run rerun <run-id> --failed`); the jobs that succeeded stay published. The `go` and `github-release` jobs wait for all five registries, so they run once the rerun succeeds. A rerun uses the workflow as it was in that run, so a fix to the workflow itself lands on `main` and ships with the next patch version. If a package is broken after it was published, release the next patch version; npm, PyPI, RubyGems, and NuGet allow deprecating or yanking the broken one, and Maven Central does not allow removal.
