# Samba server for the end-to-end tests.
FROM ubuntu:24.04
ARG DEBIAN_FRONTEND=noninteractive
RUN apt-get update \
    && apt-get install -y --no-install-recommends samba \
    && rm -rf /var/lib/apt/lists/*
COPY smb.conf /etc/samba/smb.conf
# A user for authenticated mounts (password: evos); guests are allowed too.
RUN useradd -M -s /usr/sbin/nologin evos \
    && (echo evos; echo evos) | smbpasswd -a -s evos
EXPOSE 445
CMD ["smbd", "--foreground", "--no-process-group", "--debug-stdout"]
