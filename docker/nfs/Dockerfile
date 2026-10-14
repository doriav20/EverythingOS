# User-space NFSv4 server (nfs-ganesha) for the end-to-end tests: the
# Docker Desktop kernel has no nfsd, but ganesha needs no kernel support.
FROM ubuntu:24.04
ARG DEBIAN_FRONTEND=noninteractive
RUN apt-get update \
    && apt-get install -y --no-install-recommends nfs-ganesha nfs-ganesha-vfs \
    && rm -rf /var/lib/apt/lists/* \
    && mkdir -p /var/run/ganesha /var/lib/nfs/ganesha
COPY ganesha.conf /etc/ganesha/ganesha.conf
EXPOSE 2049
CMD ["ganesha.nfsd", "-F", "-L", "/dev/stdout", "-f", "/etc/ganesha/ganesha.conf"]
