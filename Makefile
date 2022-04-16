dockerup:
	docker-compose -f ci/docker-compose.yml up -d

dockerdown:
	docker-compose -f ci/docker-compose.yml down

tests: ## Clean and Make unit tests
	python -m pytest fsspec_data --cov=fsspec_data --junitxml=python_junit.xml --cov-report=xml --cov-branch

lint: ## run linter
	python -m flake8 fsspec_data setup.py docs/conf.py

fix:  ## run black fix
	python -m black fsspec_data/ setup.py docs/conf.py

check: checks
checks:  ## run lint and other checks
	check-manifest -v

build:  ## build python
	python setup.py build 

develop:  ## install to site-packages in editable mode
	python -m pip install --upgrade build pip setuptools twine wheel
	python -m pip install -e .[develop]

install:  ## install to site-packages
	python -m pip install .

docs:  ## make documentation
	make -C ./docs html
	open ./docs/_build/html/index.html

dist:  ## create dists
	rm -rf dist build
	python setup.py sdist bdist_wheel
	python -m twine check dist/*
	
publish: dist  ## dist to pypi
	python -m twine upload dist/* --skip-existing

clean: ## clean the repository
	find . -name "__pycache__" | xargs  rm -rf 
	find . -name "*.pyc" | xargs rm -rf 
	find . -name ".ipynb_checkpoints" | xargs  rm -rf 
	rm -rf .coverage cover htmlcov logs build dist *.egg-info
	rm -rf ./*.gv*
	make -C ./docs clean

# Thanks to Francoise at marmelab.com for this
.DEFAULT_GOAL := help
help:
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | sort | awk 'BEGIN {FS = ":.*?## "}; {printf "\033[36m%-30s\033[0m %s\n", $$1, $$2}'

print-%:
	@echo '$*=$($*)'

.PHONY: tests lint fix checks check build develop install dist publish docs clean help
