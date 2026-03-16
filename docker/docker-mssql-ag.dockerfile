FROM mcr.microsoft.com/mssql/server:2022-latest

USER root
RUN mkdir -p /certs && chown mssql:root /certs
RUN mkdir -p /tmp/ag-certs && chown mssql:root /tmp/ag-certs
COPY --chown=mssql:root --chmod=444 certs/server.* /certs/
COPY --chown=mssql:root --chmod=444 certs/customCA.* /certs/
COPY --chown=mssql docker-mssql-ag.conf /var/opt/mssql/mssql.conf
USER mssql
