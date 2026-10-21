# Voltage images

This directory holds the container images that Voltage builds from this fork for Buzz remote agents.
It is Voltage-only, so upstream block/buzz syncs do not conflict with it.
The agent images moved here from voltagecloud/images at `f944157`.
The same workflow also builds the Buzz relay image from the upstream root `Dockerfile`; see [The relay image](#the-relay-image).

| Image | Contents | ECR repository |
| --- | --- | --- |
| [buzz-agent](buzz-agent/README.md) | Remote developer agent Sandbox: the `sprig` harness, Codex and Claude ACP adapters, and developer tools. It also runs as the `buzz-agent-gate` DaemonSet. | `voltage-buzz-agent-staging` |
| [buzz-sandbox-reaper](buzz-sandbox-reaper/Dockerfile) | In-cluster reaper for ended and orphaned agent Sandboxes (`buzz-sandbox-reaper reconcile`) | `voltage-buzz-sandbox-reaper-staging` |
| [buzz-relay](#the-relay-image) | Buzz relay, admin, and pair relay binaries with the web bundles, from the root `Dockerfile` | `buzz-relay` |

Each image compiles the Buzz crates of the commit it is built from.
The build context is the repository root.
For the two agent images, BuildKit reads the `Dockerfile.dockerignore` next to each Dockerfile instead of the root `.dockerignore`.
These files keep only the Cargo workspace and, for `buzz-agent`, the files that its Dockerfile copies from `voltage/images/buzz-agent`.

## Publishing

[`.github/workflows/voltage-agent-images.yml`](../../.github/workflows/voltage-agent-images.yml) runs when a change touches the files that an image reads.
Its first job selects only the images whose inputs changed; manual runs build all of them.

- Pull requests: hadolint and BuildKit checks, then a build of each selected image for `linux/amd64` and `linux/arm64` on native runners, without a push.
  The workflow tests the agent image on both platforms, runs the reaper binary, and smoke-tests the relay image with [`test-relay-image.sh`](test-relay-image.sh).
  These jobs have no AWS access.
- `main`: each platform builds on its native runner and is pushed by digest.
  A merge job then creates one manifest list per image, tagged with the full commit SHA.
  ECR tags are immutable; if the tag exists, the job reports it and does not replace it.
  The job summary lists the manifest list digest for each image.
- Manual runs (`workflow_dispatch`) publish only on `main`.

The voltage-terraform `v2/environments/staging` root creates the two agent ECR repositories; `v2/environments/prod` creates `buzz-relay`.
Publishing assumes the `github-buzz-ecr-push` IAM role (voltage-terraform `v2/environments/github`).
The role trusts only `repo:voltagecloud/buzz:ref:refs/heads/main` and can push to and read only these three repositories.
The repository secret `AWS_ACCOUNT_ID` supplies the account ID, so the account ID is not in the repository and is masked in the public logs.

## Deploying a digest

Pin the manifest list digest from the job summary (`<repository>@sha256:<digest>`), never a tag or a local image ID.

| Image | Where to pin it |
| --- | --- |
| `voltage-buzz-agent-staging` | The Buzz Kubernetes provider `image`, and `image.digest` in voltagecloud/cd `staging/values/infra-utils/buzz-agent-gate.yaml` |
| `voltage-buzz-sandbox-reaper-staging` | `image.digest` in voltagecloud/cd `staging/values/infra-utils/buzz-sandbox-reaper.yaml` |
| `buzz-relay` | `image.repository` and `image.digest` in voltagecloud/cd `prod/values/buzz/prod.yaml` |

The voltage-terraform admission policy admits only digest-pinned images from `voltage-buzz-agent-staging` in developer namespaces.

## Build and test locally

Run from the repository root. The Rust build takes a long time; the Dockerfiles set `CARGO_BUILD_JOBS=2`.

```bash
docker buildx build --check -f voltage/images/buzz-agent/Dockerfile .
docker buildx build --check -f voltage/images/buzz-sandbox-reaper/Dockerfile .

docker build --platform linux/amd64 -f voltage/images/buzz-sandbox-reaper/Dockerfile -t buzz-sandbox-reaper:local .
docker run --rm buzz-sandbox-reaper:local --help

docker build --platform linux/amd64 -f voltage/images/buzz-agent/Dockerfile -t buzz-agent:local .
bash voltage/images/buzz-agent/test-image.sh buzz-agent:local
```

[buzz-agent/README.md](buzz-agent/README.md#build-and-test) lists the other agent image tests.

## The relay image

The relay image is the upstream Buzz relay: the root [`Dockerfile`](../../Dockerfile), target `runtime`, with the root `.dockerignore`.
Upstream publishes it to GHCR with [`.github/workflows/docker.yml`](../../.github/workflows/docker.yml); this fork does not change that file or the Dockerfile.
The Voltage workflow builds the same Dockerfile with the same `BUZZ_SOURCE_SHA`, `BUZZ_BUILD_ID`, and `BUZZ_BUILD_URL` build arguments and pushes it to the `buzz-relay` ECR repository, tagged with the full commit SHA.
It does not publish the `runtime-debug` variant.
Production runs the relay from this repository, so prod pins the `buzz-relay` manifest list digest in voltagecloud/cd.

```bash
docker buildx build --check .
docker build --platform linux/amd64 --target runtime -t buzz-relay:local .
bash voltage/images/test-relay-image.sh buzz-relay:local
```
