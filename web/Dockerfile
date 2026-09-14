# Builds the Angular dashboard (`ng build`) and serves the static output
# through nginx — see nginx.conf for the /edh/v1/* reverse proxy that
# replaces what `ng serve`'s dev proxy (proxy.conf.json) does for local dev,
# needed here since a static build has no dev server of its own to do that
# proxying.

FROM node:22-alpine AS build
WORKDIR /app
COPY package.json package-lock.json ./
RUN npm ci
COPY . .
RUN npm run build

FROM nginx:1.27-alpine
COPY nginx.conf /etc/nginx/conf.d/default.conf
COPY --from=build /app/dist/web/browser /usr/share/nginx/html
