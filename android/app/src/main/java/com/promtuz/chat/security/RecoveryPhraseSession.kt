package com.promtuz.chat.security

import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch

internal sealed interface RecoveryPhraseState {
    data class Locked(val notice: RecoveryNotice? = null) : RecoveryPhraseState
    data object Authenticating : RecoveryPhraseState
    data object AwaitingForeground : RecoveryPhraseState
    data object Loading : RecoveryPhraseState
    // Deliberately not a data class: toString must never print the secret.
    class Revealed(val words: List<String>) : RecoveryPhraseState
}

internal enum class RecoveryNotice { Cancelled, Hidden, AuthenticationFailed, LoadFailed, Unavailable }

/** Screen-scoped, main-thread state. Neither words nor authorization enter saved state. */
internal class RecoveryPhraseSession(
    private val scope: CoroutineScope,
    private val exportPhrase: suspend () -> List<String>,
) {
    private val mutableState = MutableStateFlow<RecoveryPhraseState>(RecoveryPhraseState.Locked())
    val state = mutableState.asStateFlow()
    private var generation = 0
    private var foreground = false
    private var closed = false
    private var loadJob: Job? = null

    fun beginAuthentication(): Int? {
        if (closed || !foreground || mutableState.value !is RecoveryPhraseState.Locked) return null
        mutableState.value = RecoveryPhraseState.Authenticating
        return ++generation
    }

    fun authenticated(attempt: Int) {
        if (closed || attempt != generation || mutableState.value != RecoveryPhraseState.Authenticating) return
        mutableState.value = RecoveryPhraseState.AwaitingForeground
        if (foreground) load()
    }

    fun authenticationFailed(attempt: Int, notice: RecoveryNotice) {
        if (closed || attempt != generation || mutableState.value != RecoveryPhraseState.Authenticating) return
        lock(notice)
    }

    fun setForeground(value: Boolean) {
        foreground = value
        if (closed) return
        if (value && mutableState.value == RecoveryPhraseState.AwaitingForeground) load()
        if (!value && (mutableState.value is RecoveryPhraseState.Revealed ||
                    mutableState.value == RecoveryPhraseState.Loading ||
                    mutableState.value == RecoveryPhraseState.AwaitingForeground)) {
            lock(RecoveryNotice.Hidden)
        }
        // Android's credential Activity can stop this screen while authenticating.
        // Its result may authorize a reveal only after we return to the foreground.
    }

    fun lock(notice: RecoveryNotice? = RecoveryNotice.Hidden) {
        generation++
        loadJob?.cancel()
        loadJob = null
        mutableState.value = RecoveryPhraseState.Locked(notice)
    }

    fun close() {
        closed = true
        foreground = false
        lock(null)
    }

    private fun load() {
        val attempt = generation
        mutableState.value = RecoveryPhraseState.Loading
        loadJob = scope.launch {
            try {
                val words = exportPhrase()
                require(words.size == 24 && words.all { it.isNotBlank() && it.none(Char::isWhitespace) })
                if (!closed && foreground && attempt == generation) {
                    mutableState.value = RecoveryPhraseState.Revealed(words)
                    delay(120_000)
                    if (attempt == generation) lock(RecoveryNotice.Hidden)
                }
            } catch (e: CancellationException) {
                throw e
            } catch (_: Exception) {
                if (!closed && attempt == generation) lock(RecoveryNotice.LoadFailed)
            }
        }
    }
}
