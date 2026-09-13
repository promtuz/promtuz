package com.promtuz.chat.security

import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.NonCancellable
import kotlinx.coroutines.test.runCurrent
import kotlinx.coroutines.test.advanceTimeBy
import kotlinx.coroutines.test.runTest
import kotlinx.coroutines.withContext
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

@OptIn(ExperimentalCoroutinesApi::class)
class RecoveryPhraseSessionTest {
    // Synthetic fixture, not an exported identity or a usable recovery phrase.
    private val words = List(24) { "example" }

    @Test fun openingAndCancellingNeverExportsAndRepeatedTapsAreIgnored() = runTest {
        var exports = 0
        val session = RecoveryPhraseSession(backgroundScope) { exports++; words }
        assertNull(session.beginAuthentication())
        session.setForeground(true)
        runCurrent()
        assertEquals(0, exports)
        val attempt = session.beginAuthentication()!!
        assertNull(session.beginAuthentication())
        session.authenticationFailed(attempt, RecoveryNotice.Cancelled)
        session.authenticated(attempt)
        runCurrent()
        assertEquals(0, exports)
        assertEquals(RecoveryPhraseState.Locked(RecoveryNotice.Cancelled), session.state.value)
    }

    @Test fun credentialActivityCanStopScreenButExportWaitsForReturn() = runTest {
        var exports = 0
        val session = RecoveryPhraseSession(backgroundScope) { exports++; words }
        session.setForeground(true)
        val attempt = session.beginAuthentication()!!
        session.setForeground(false)
        session.authenticated(attempt)
        runCurrent()
        assertEquals(0, exports)
        assertEquals(RecoveryPhraseState.AwaitingForeground, session.state.value)
        session.setForeground(true)
        runCurrent()
        assertEquals(1, exports)
        assertEquals(words, (session.state.value as RecoveryPhraseState.Revealed).words)
    }

    @Test fun backgroundingRevealedWordsRequiresFreshAuthentication() = runTest {
        var exports = 0
        val session = RecoveryPhraseSession(backgroundScope) { exports++; words }
        session.setForeground(true)
        val attempt = session.beginAuthentication()!!
        session.authenticated(attempt)
        runCurrent()
        session.setForeground(false)
        session.setForeground(true)
        session.authenticated(attempt)
        runCurrent()
        assertEquals(1, exports)
        assertEquals(RecoveryPhraseState.Locked(RecoveryNotice.Hidden), session.state.value)
    }

    @Test fun lateNonCooperativeExportCannotRevealAfterBackgrounding() = runTest {
        val result = CompletableDeferred<List<String>>()
        val session = RecoveryPhraseSession(backgroundScope) { withContext(NonCancellable) { result.await() } }
        session.setForeground(true)
        session.authenticated(session.beginAuthentication()!!)
        runCurrent()
        assertEquals(RecoveryPhraseState.Loading, session.state.value)
        session.setForeground(false)
        session.setForeground(true)
        result.complete(words)
        runCurrent()
        assertEquals(RecoveryPhraseState.Locked(RecoveryNotice.Hidden), session.state.value)
    }

    @Test fun oldPromptCannotAuthorizeNewAttemptOrOverwriteItsResult() = runTest {
        val session = RecoveryPhraseSession(backgroundScope) { words }
        session.setForeground(true)
        val first = session.beginAuthentication()!!
        session.lock()
        val second = session.beginAuthentication()!!
        session.authenticated(first)
        assertEquals(RecoveryPhraseState.Authenticating, session.state.value)
        session.authenticated(second)
        runCurrent()
        session.authenticationFailed(first, RecoveryNotice.AuthenticationFailed)
        assertTrue(session.state.value is RecoveryPhraseState.Revealed)
    }

    @Test fun disposingIgnoresAuthenticationResultsAndClearsWords() = runTest {
        var exports = 0
        val session = RecoveryPhraseSession(backgroundScope) { exports++; words }
        session.setForeground(true)
        val attempt = session.beginAuthentication()!!
        session.close()
        session.authenticated(attempt)
        session.setForeground(true)
        runCurrent()
        assertEquals(0, exports)
        assertNull(session.beginAuthentication())
        assertTrue(session.state.value is RecoveryPhraseState.Locked)
    }

    @Test fun malformedExportAndCoreErrorsStayHiddenWithoutExposingExceptionText() = runTest {
        for (invalid in listOf(emptyList(), List(23) { "example" }, List(24) { " " }, List(24) { "two words" })) {
            val session = RecoveryPhraseSession(backgroundScope) { invalid }
            session.setForeground(true)
            session.authenticated(session.beginAuthentication()!!)
            runCurrent()
            assertEquals(RecoveryPhraseState.Locked(RecoveryNotice.LoadFailed), session.state.value)
        }
        val session = RecoveryPhraseSession(backgroundScope) { error("sensitive internal details") }
        session.setForeground(true)
        session.authenticated(session.beginAuthentication()!!)
        runCurrent()
        assertEquals(RecoveryPhraseState.Locked(RecoveryNotice.LoadFailed), session.state.value)
        assertFalse(session.state.value.toString().contains("sensitive"))
    }

    @Test fun revealedStateDoesNotIncludeWordsInToString() {
        assertFalse(RecoveryPhraseState.Revealed(words).toString().contains("example"))
    }

    @Test fun visibilityExpiresAfterTwoMinutesAndCannotReuseAuthorization() = runTest {
        val session = RecoveryPhraseSession(backgroundScope) { words }
        session.setForeground(true)
        val attempt = session.beginAuthentication()!!
        session.authenticated(attempt)
        runCurrent()
        advanceTimeBy(119_999)
        assertTrue(session.state.value is RecoveryPhraseState.Revealed)
        advanceTimeBy(1)
        runCurrent()
        assertEquals(RecoveryPhraseState.Locked(RecoveryNotice.Hidden), session.state.value)
        session.authenticated(attempt)
        assertTrue(session.state.value is RecoveryPhraseState.Locked)
    }
}
