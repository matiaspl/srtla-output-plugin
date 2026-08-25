FROM --platform=linux/arm64 ubuntu:latest AS build

ENV DEBIAN_FRONTEND=noninteractive
RUN apt update && apt install cmake make wget xz-utils git build-essential libssl-dev openssl -y

WORKDIR /libsrt
COPY . .

RUN mkdir /out
RUN cmake -DCMAKE_INSTALL_PREFIX=/usr ./
RUN make -j
RUN make DESTDIR=/out install

FROM scratch AS export
COPY --from=build /out /
