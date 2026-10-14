# syntax=docker/dockerfile:1
FROM eclipse-temurin:21-jre
ARG AERON_VERSION=1.53.3
# Pinned by content: the sha256 of aeron-all-1.53.3.jar, whose sha1 2ffcf6acba37da20d475e27f2896a1a52cf42de3
# matches Maven Central. A different AERON_VERSION needs its own checksum here.
ADD --checksum=sha256:4c6c8a5014a2b67424051170392a005daa9a998ac963302d2819d0f3db749ef2 \
  https://repo1.maven.org/maven2/io/aeron/aeron-all/${AERON_VERSION}/aeron-all-${AERON_VERSION}.jar /opt/aeron/aeron-all.jar
# Not root. The driver creates its files in /dev/shm/aeron as 0644, owned by this uid, and the Rust
# services (docker/rust.Dockerfile) map them read-write, so they run as the same uid 10001.
# The Archive volume is initialised from /data/archive, so it starts owned by that uid too.
RUN chmod 0644 /opt/aeron/aeron-all.jar \
    && groupadd --system --gid 10001 aeron \
    && useradd --system --uid 10001 --gid 10001 --no-create-home --shell /usr/sbin/nologin aeron \
    && mkdir -p /data/archive && chown 10001:10001 /data/archive
USER 10001:10001
# file.sync.level=2 / catalog.file.sync.level=2: the Archive fsyncs (data and metadata) each block
# before it advances the recording position that `ArchiveJournal::wait_durable` reads, so "durable"
# survives a host crash or power loss, not just a process crash. The cost is an fsync per recorded
# block on the write-ahead path (one wait per batch of up to MAX_BATCH commands).
ENTRYPOINT ["java", "--add-opens", "java.base/jdk.internal.misc=ALL-UNNAMED", "--add-opens", "java.base/sun.nio.ch=ALL-UNNAMED", \
  "-Daeron.dir=/dev/shm/aeron", "-Daeron.dir.delete.on.start=true", \
  "-Daeron.archive.dir=/data/archive", \
  "-Daeron.archive.file.sync.level=2", \
  "-Daeron.archive.catalog.file.sync.level=2", \
  "-Daeron.archive.control.channel=aeron:udp?endpoint=localhost:8010", \
  "-Daeron.archive.replication.channel=aeron:udp?endpoint=localhost:0", \
  "-Daeron.archive.recording.events.enabled=false", \
  "-cp", "/opt/aeron/aeron-all.jar", "io.aeron.archive.ArchivingMediaDriver"]
