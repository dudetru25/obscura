// The fork folder pins the reviewed homelab library and source revision.
@Library('homelab') _

try {
    homelabPipeline(
        agentLabel: 'homelab-dev3-linux',
        branch: 'feat/media-playback',
        sourceRepository: 'https://github.com/dudetru25/obscura.git',
        scmCredentialId: scm.userRemoteConfigs[0].credentialsId,
        sourceRevisionParameter: 'SOURCE_SHA',
        scriptPath: 'Jenkinsfile',
        pipelineRevision: params.SOURCE_SHA,
        timeoutMinutes: 60,
        queueProfile: 'small-web',
        checkCommand: 'CHROME_BIN=/mnt/g/GithubProjects/.jenkins-workspace/.tools/chrome-155.0.8059.84/chrome bash scripts/ci/jenkins.sh'
    )
} finally {
    node('homelab-dev3-linux') {
        ws("${pwd()}@build-${env.BUILD_NUMBER}") {
            archiveArtifacts(artifacts: 'ci-output/**', excludes: 'ci-output/obscura-benchmark/**',
                allowEmptyArchive: true, fingerprint: true)
        }
    }
}
