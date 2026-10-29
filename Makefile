WASM := target/wasm32-unknown-unknown/release/wasmo_wasm.wasm
WASMO_WASM ?=
WASM_SRC := $(or $(WASMO_WASM),$(WASM))
JS_DEPS := js/node_modules/.package-lock.json
RUBY_DEPS := ruby/vendor/bundle/.installed
DIST := dist

.PHONY: all wasm test test-rust test-go test-js test-jvm test-dotnet test-python test-ruby dist dist-js dist-jvm dist-dotnet dist-python dist-ruby dist-go version check-version fmt lint

all: test

wasm:
ifeq ($(WASMO_WASM),)
	cargo build --release --target wasm32-unknown-unknown -p wasmo-wasm
endif
	cp $(WASM_SRC) go/wasmo.wasm
	cp $(WASM_SRC) js/wasmo.wasm
	cp $(WASM_SRC) jvm/wasmo.wasm
	cp $(WASM_SRC) dotnet/wasmo.wasm
	cp $(WASM_SRC) python/src/wasmo/wasmo.wasm
	cp $(WASM_SRC) ruby/lib/wasmo/wasmo.wasm

$(JS_DEPS): js/package.json js/package-lock.json
	cd js && npm ci

$(RUBY_DEPS): ruby/Gemfile ruby/Gemfile.lock ruby/wasmo.gemspec
	cd ruby && BUNDLE_FROZEN=true bundle install
	touch $@

test: test-rust test-go test-js test-jvm test-dotnet test-python test-ruby

test-rust:
	cargo test --workspace

test-go: wasm
	cd go && CGO_ENABLED=0 go test ./...

test-js: wasm $(JS_DEPS)
	cd js && npm run build && npm test && npm run test:bun

test-jvm: wasm
	cd jvm && mvn -B -q verify

test-dotnet: wasm
	cd dotnet && dotnet test -c Release --logger "console;verbosity=detailed"

test-python: wasm
	cd python && python -m pytest

test-ruby: wasm $(RUBY_DEPS)
	cd ruby && bundle exec rake test

dist: dist-js dist-jvm dist-dotnet dist-python dist-ruby dist-go
	cp $(WASM_SRC) $(DIST)/wasmo.wasm
	cd $(DIST) && find . -type f ! -name SHA256SUMS | sed "s|^\./||" | LC_ALL=C sort | xargs sha256sum > SHA256SUMS

dist-js: wasm $(JS_DEPS)
	rm -rf $(DIST)/js && mkdir -p $(DIST)/js
	cd js && npm pack --pack-destination ../$(DIST)/js

dist-jvm: wasm
	rm -rf $(DIST)/jvm && mkdir -p $(DIST)/jvm
	cd jvm && mvn -B -q -P release -Dgpg.skip -DskipTests package
	v=$$(scripts/version.sh get) && cp jvm/target/wasmo-$$v.jar jvm/target/wasmo-$$v-sources.jar jvm/target/wasmo-$$v-javadoc.jar $(DIST)/jvm/ && cp jvm/pom.xml $(DIST)/jvm/wasmo-$$v.pom

dist-dotnet: wasm
	rm -rf $(DIST)/dotnet
	dotnet pack dotnet/src/Wasmo -c Release -o $(DIST)/dotnet

dist-python: wasm
	rm -rf $(DIST)/python
	cd python && python -m build --no-isolation --outdir ../$(DIST)/python

dist-ruby: wasm
	rm -rf $(DIST)/ruby && mkdir -p $(DIST)/ruby
	cd ruby && gem build wasmo.gemspec --output ../$(DIST)/ruby/wasmo-$$(../scripts/version.sh get).gem

dist-go: wasm
	scripts/go-release.sh $(WASM_SRC)

version:
	scripts/version.sh set

check-version:
	scripts/version.sh check

fmt: $(JS_DEPS) $(RUBY_DEPS)
	cargo fmt --all
	cd go && gofmt -w .
	cd js && npm run format
	cd jvm && mvn -B -q spotless:apply
	cd dotnet && dotnet format
	cd python && ruff check --fix . && ruff format .
	cd ruby && bundle exec rubocop -a

lint: wasm $(JS_DEPS) $(RUBY_DEPS)
	scripts/version.sh check
	actionlint
	shellcheck scripts/*.sh
	cargo fmt --all -- --check
	cargo clippy --workspace --all-targets -- -D warnings
	cd go && test -z "$$(gofmt -l .)" && go vet ./...
	cd js && npm run build && npm run typecheck && npm run format:check
	cd jvm && mvn -B -q spotless:check test-compile
	cd dotnet && dotnet format --verify-no-changes && dotnet build -c Release --no-incremental
	cd python && ruff check . && ruff format --check .
	cd ruby && bundle exec rubocop
