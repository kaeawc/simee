# The site is static: GitHub Actions builds website/dist (scripts/build.sh), and this image only
# serves it, so a deploy uploads a few MB and builds nothing on Fly.
FROM caddy:2.11.9-alpine
COPY Caddyfile /etc/caddy/Caddyfile
COPY dist /srv
EXPOSE 8080
