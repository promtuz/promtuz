package com.promtuz.chat.presentation.viewmodel

import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import com.promtuz.core.CoreBridge
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import timber.log.Timber
import uniffi.core.ContactDiag

/** Newest message's delivery state (mirrors libcore's status ints). */
enum class SendState { PENDING, SENT, FAILED }

/**
 * Screen model: uniffi's unsigned types flattened to plain Kotlin. `ipkHex`
 * is both the list key and what's handed back to `forgetContact` (decoded to
 * bytes), so no `ByteArray` lives in the data class — keeps equality sane.
 */
data class UiContact(
    val ipkHex: String,
    val name: String,
    val paired: Boolean,
    val epoch: Long?,
    val messageCount: Int,
    val lastStatus: SendState?,
    val pendingOps: Int,
)

/**
 * Polls the contact set every [POLL_MS] for a live diagnostics view — as you
 * re-pair and send, you watch a contact go unpaired→paired→epoch N and its
 * message status advance. Screen-scoped, so polling stops when popped.
 */
class ContactsVM : ViewModel() {
    private val _contacts = MutableStateFlow<List<UiContact>>(emptyList())
    val contacts: StateFlow<List<UiContact>> = _contacts.asStateFlow()

    init {
        viewModelScope.launch {
            while (isActive) {
                refresh()
                delay(POLL_MS)
            }
        }
    }

    private suspend fun refresh() {
        try {
            _contacts.value = CoreBridge.contactsDiag()
                .map { it.toUi() }
                .sortedBy { it.name.lowercase() }
        } catch (e: Exception) {
            Timber.tag("ContactsVM").e(e, "Failed to load contacts")
        }
    }

    /** Wipe the contact + all its state; the list refreshes right after. */
    fun forget(ipkHex: String) = act { CoreBridge.forgetContact(ipkHex.hexToBytes()) }

    private inline fun act(crossinline block: suspend () -> Unit) {
        viewModelScope.launch {
            try {
                block()
            } catch (e: Exception) {
                Timber.tag("ContactsVM").e(e, "Contact action failed")
            }
            refresh()
        }
    }

    private fun ContactDiag.toUi(): UiContact {
        val state = when (lastStatus?.toInt()) {
            0 -> SendState.PENDING
            1 -> SendState.SENT
            2 -> SendState.FAILED
            else -> null
        }
        return UiContact(
            ipkHex = ipk.toHex(),
            name = name,
            paired = paired,
            epoch = epoch?.toLong(),
            messageCount = messageCount.toInt(),
            lastStatus = state,
            pendingOps = pendingOps.toInt(),
        )
    }

    companion object {
        private const val POLL_MS = 1500L
    }
}

private fun ByteArray.toHex(): String = joinToString("") { "%02x".format(it) }

private fun String.hexToBytes(): ByteArray =
    chunked(2).map { it.toInt(16).toByte() }.toByteArray()
