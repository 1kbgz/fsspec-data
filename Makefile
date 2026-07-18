#########
# BUILD #
#########
.PHONY: develop-py develop-rs develop
develop-py:
	uv pip install -e .[develop]

develop-rs:
	make -C rust develop

develop: develop-rs develop-py  ## setup project for development

.PHONY: requirements-py requirements-rs requirements
requirements-py:  ## install prerequisite python build requirements
	python -m pip install --upgrade pip toml
	python -m pip install `python -c 'import toml; c = toml.load("pyproject.toml"); print("\n".join(c["build-system"]["requires"]))'`
	python -m pip install `python -c 'import toml; c = toml.load("pyproject.toml"); print(" ".join(c["project"]["optional-dependencies"]["develop"]))'`

requirements-rs:  ## install prerequisite rust build requirements
	make -C rust requirements

requirements: requirements-rs requirements-py  ## setup project for development

.PHONY: build-py build-rs build
build-py:
	python -m build -w -n

build-rs:
	make -C rust build

build: build-rs build-py  ## build the project

.PHONY: install
install:  ## install python library
	uv pip install .

#########
# LINTS #
#########
.PHONY: lint-py lint-rs lint-docs lint lints
lint-py:  ## run python linter with ruff
	python -m ruff check fsspec_data
	python -m ruff format --check fsspec_data

lint-rs:  ## run rust linter
	make -C rust lint

lint-docs:  ## lint docs with mdformat and codespell
	python -m mdformat --check README.md docs/tutorial.md docs/how-to-integrate.md docs/how-to-chain-filesystems.md docs/explanation.md docs/api.md
	python -m codespell_lib README.md docs/tutorial.md docs/how-to-integrate.md docs/how-to-chain-filesystems.md docs/explanation.md docs/api.md

lint: lint-rs lint-py lint-docs  ## run project linters

# alias
lints: lint

.PHONY: fix-py fix-rs fix-docs fix format
fix-py:  ## fix python formatting with ruff
	python -m ruff check --fix fsspec_data
	python -m ruff format fsspec_data

fix-rs:  ## fix code with rustfmt
	make -C rust fix

fix-docs:  ## autoformat docs with mdformat and codespell
	python -m mdformat README.md docs/tutorial.md docs/how-to-integrate.md docs/how-to-chain-filesystems.md docs/explanation.md docs/api.md
	python -m codespell_lib --write README.md docs/tutorial.md docs/how-to-integrate.md docs/how-to-chain-filesystems.md docs/explanation.md docs/api.md

fix: fix-rs fix-py fix-docs  ## run project autoformatters

# alias
format: fix

################
# Other Checks #
################
.PHONY: check-dist check-types checks check

check-dist:  ## check python sdist and wheel with check-dist
	check-dist --rebuild -v

check-types:  ## check python types with ty
	ty check --python $$(which python)

checks: check-dist

# alias
check: checks

#########
# TESTS #
#########
.PHONY: test-py tests-py coverage-py
test-py:  ## run python tests
	python -m pytest -v fsspec_data/tests

# alias
tests-py: test-py

coverage-py:  ## run python tests and collect test coverage
	python -m pytest -v fsspec_data/tests --cov=fsspec_data --cov-report term-missing --cov-report xml

.PHONY: test-rs tests-rs coverage-rs
test-rs:  ## run rust tests
	make -C rust test

# alias
tests-rs: test-rs

coverage-rs:  ## run Rust tests and collect coverage
	make -C rust coverage

.PHONY: test coverage tests
test: test-py test-rs  ## run all tests
coverage: coverage-py coverage-rs  ## run all tests and collect test coverage

# alias
tests: test

###########
# VERSION #
###########
.PHONY: show-version patch minor major

show-version:  ## show current library version
	@bump-my-version show current_version

patch:  ## bump a patch version
	@bump-my-version bump patch

minor:  ## bump a minor version
	@bump-my-version bump minor

major:  ## bump a major version
	@bump-my-version bump major

########
# DIST #
########
.PHONY: dist-py-wheel dist-py-sdist dist-rs dist-check dist publish

dist-py-wheel:  ## build python wheels
	python -m cibuildwheel --output-dir dist

dist-py-sdist:  ## build Python sdist
	python -m build --sdist -o dist

dist-rs:  ## validate Rust crate distribution
	make -C rust dist

dist-check:  ## run Python dist checker with twine
	python -m twine check dist/*

dist: clean build dist-rs dist-py-wheel dist-py-sdist dist-check  ## build all dists

publish: dist  ## publish python assets

#########
# CLEAN #
#########
.PHONY: deep-clean clean

deep-clean: ## clean everything in the repository
	git clean -fdx

clean: ## clean generated build artifacts
	rm -rf .coverage coverage cover htmlcov logs build dist *.egg-info

############################################################################################

.PHONY: help

# Thanks to Francoise at marmelab.com for this
.DEFAULT_GOAL := help
help:
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | sort | awk 'BEGIN {FS = ":.*?## "}; {printf "\033[36m%-30s\033[0m %s\n", $$1, $$2}'

print-%:
	@echo '$*=$($*)'
