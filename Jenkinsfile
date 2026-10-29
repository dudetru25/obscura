// The fork folder pins the reviewed homelab library and source revision.
@Library('homelab') _

try {
    homelabPipeline(
        agentLabel: 'ci-dev-3-instabox-3',
        branch: 'fix/browser-api-compat',
        sourceRepository: 'https://github.com/dudetru25/obscura.git',
        scmCredentialId: scm.userRemoteConfigs[0].credentialsId,
        sourceRevisionParameter: 'SOURCE_SHA',
        scriptPath: 'Jenkinsfile',
        pipelineRevision: params.SOURCE_SHA,
        timeoutMinutes: 60,
        queueProfile: 'small-web',
        checkCommand: 'CHROME_BIN="$HOME/.tools/chrome-155.0.8059.84/chrome" bash scripts/ci/jenkins.sh'
    )
} finally {
    node('ci-dev-3-instabox-3') {
        ws("${pwd()}@build-${env.BUILD_NUMBER}") {
            archiveArtifacts(artifacts: 'ci-output/**', excludes: 'ci-output/obscura-benchmark/**',
                allowEmptyArchive: true, fingerprint: true)
        }
    }
}
