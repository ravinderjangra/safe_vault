SHELL := /bin/bash
SAFE_VAULT_VERSION := $(shell grep "^version" < Cargo.toml | head -n 1 | awk '{ print $$3 }' | sed 's/\"//g')
COMMIT_HASH := $(shell git rev-parse --short HEAD)
USER_ID := $(shell id -u)
GROUP_ID := $(shell id -g)
UNAME_S := $(shell uname -s)
PWD := $(shell echo $$PWD)
UUID := $(shell uuidgen | sed 's/-//g')
DEPLOY_PATH := deploy
DEPLOY_PROD_PATH := ${DEPLOY_PATH}/prod

package-commit_hash-artifacts-for-deploy:
	rm -f *.tar
	rm -rf ${DEPLOY_PATH}
	mkdir -p ${DEPLOY_PROD_PATH}

	tar -C artifacts/prod/x86_64-unknown-linux-musl/release \
        -cvf safe_vault-${COMMIT_HASH}-x86_64-unknown-linux-musl.tar safe_vault
	tar -C artifacts/prod/x86_64-pc-windows-gnu/release \
        -cvf safe_vault-${COMMIT_HASH}-x86_64-pc-windows-gnu.tar safe_vault.exe
	tar -C artifacts/prod/x86_64-apple-darwin/release \
        -cvf safe_vault-${COMMIT_HASH}-x86_64-apple-darwin.tar safe_vault

	mv *.tar ${DEPLOY_PROD_PATH}

.ONESHELL:
package-version-artifacts-for-deploy:
	rm -f *.zip *.tar.gz
	rm -rf ${DEPLOY_PATH}
	mkdir -p ${DEPLOY_PROD_PATH}

	zip -j safe_vault-${SAFE_VAULT_VERSION}-x86_64-unknown-linux-musl.zip \
		artifacts/prod/x86_64-unknown-linux-musl/release/safe_vault
	zip -j safe_vault-${SAFE_VAULT_VERSION}-x86_64-pc-windows-gnu.zip \
		artifacts/prod/x86_64-pc-windows-gnu/release/safe_vault.exe
	zip -j safe_vault-${SAFE_VAULT_VERSION}-x86_64-apple-darwin.zip \
		artifacts/prod/x86_64-apple-darwin/release/safe_vault

	tar -C artifacts/prod/x86_64-unknown-linux-musl/release \
		-zcvf safe_vault-${SAFE_VAULT_VERSION}-x86_64-unknown-linux-musl.tar.gz safe_vault
	tar -C artifacts/prod/x86_64-pc-windows-gnu/release \
		-zcvf safe_vault-${SAFE_VAULT_VERSION}-x86_64-pc-windows-gnu.tar.gz safe_vault.exe
	tar -C artifacts/prod/x86_64-apple-darwin/release \
		-zcvf safe_vault-${SAFE_VAULT_VERSION}-x86_64-apple-darwin.tar.gz safe_vault

	mv *.zip ${DEPLOY_PROD_PATH}
	mv *.tar.gz ${DEPLOY_PROD_PATH}
