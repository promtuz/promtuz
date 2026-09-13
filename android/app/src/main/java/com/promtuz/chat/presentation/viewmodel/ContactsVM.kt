package com.promtuz.chat.presentation.viewmodel

import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import com.promtuz.chat.navigation.Routes
import com.promtuz.chat.utils.extensions.fromHex
import com.promtuz.chat.utils.extensions.toHex
import com.promtuz.core.CoreBridge
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch
import timber.log.Timber

class ContactsVM(private val app: AppVM) : ViewModel() {
    private val _busy = MutableStateFlow(false)
    val busy = _busy.asStateFlow()
    private val _error = MutableStateFlow<String?>(null)
    val error = _error.asStateFlow()
    fun clearError() { _error.value = null }

    fun open(person: UiMember) = act("Couldn’t open the chat. Try again.") {
        val conversation = CoreBridge.conversationWith(person.ipkHex.fromHex())
        if (app.backStack.lastOrNull() == Routes.Contacts) {
            app.backStack[app.backStack.lastIndex] = Routes.Chat(conversation.toHex(), person.name)
        }
    }

    fun delete(person: UiMember, onComplete: () -> Unit) =
        act("Couldn’t delete the contact. Try again.") {
            CoreBridge.forgetContact(person.ipkHex.fromHex())
            onComplete()
        }

    private fun act(failure: String, block: suspend () -> Unit) {
        if (_busy.value) return
        _busy.value = true
        _error.value = null
        viewModelScope.launch {
            try { block() }
            catch (e: CancellationException) { throw e }
            catch (e: Exception) {
                Timber.e(e)
                _error.value = failure
            } finally { _busy.value = false }
        }
    }
}
