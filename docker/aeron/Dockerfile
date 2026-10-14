FROM eclipse-temurin:21-jre
ARG AERON_VERSION=1.53.3
ADD https://repo1.maven.org/maven2/io/aeron/aeron-all/${AERON_VERSION}/aeron-all-${AERON_VERSION}.jar /opt/aeron/aeron-all.jar
ENTRYPOINT ["java", "--add-opens", "java.base/jdk.internal.misc=ALL-UNNAMED", "--add-opens", "java.base/sun.nio.ch=ALL-UNNAMED", \
  "-Daeron.dir=/dev/shm/aeron", "-Daeron.dir.delete.on.start=true", \
  "-Daeron.archive.dir=/data/archive", \
  "-Daeron.archive.control.channel=aeron:udp?endpoint=localhost:8010", \
  "-Daeron.archive.replication.channel=aeron:udp?endpoint=localhost:0", \
  "-Daeron.archive.recording.events.enabled=false", \
  "-cp", "/opt/aeron/aeron-all.jar", "io.aeron.archive.ArchivingMediaDriver"]
