# URL Shortner

it is scalable URL shortner service using:
* Rust: as programming language
* Docker: as containerization technology and deployment
* Redis: as cache server
* ScyllaDB: NoSQL servers to hold state
* NGINX: As reverse proxy, Load balancer, rate limiter and HTTPS terminal
* Prometheus: As stats collector
* Grafana: Stats and observability dashboard server

The current Topology
- 2x web server
- 2x ScyllaDB servers
- 1x Redis
- 2x NGINX
- 1x Prometheus
- 1x Grafana

# Deploying
```bash
cd urlshort
docker build . -f .\urlshort.Dockerfile -t urlshort-server
docker compose up
```

