# short link API — Axum + SQLx

Track: <https://hackingwithrust.com/projects/axum-sqlx-api/>

**Status: not started.** The first commit lands with article 2, *Setting up Axum and your first route*.

Tags: `api-ch01` … `api-ch15`.

Needs a Postgres instance:

```sh
docker run --name shortlink-pg -e POSTGRES_PASSWORD=dev -p 5432:5432 -d postgres:16
```

See [`../README.md`](../README.md) for what the tags mean.
