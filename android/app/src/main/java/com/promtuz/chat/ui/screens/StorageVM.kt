package com.promtuz.chat.ui.screens

import androidx.compose.material3.SnackbarHostState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import com.promtuz.chat.data.storage.StorageMediaItem
import com.promtuz.chat.data.storage.StorageSource
import com.promtuz.chat.data.storage.StorageUsage
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.launch

/** Retain the inventory while a navigation entry is covered, including during predictive back. */
internal class StorageVM(private val source: StorageSource) : ViewModel() {
    var usage by mutableStateOf<StorageUsage?>(null)
        private set
    var media by mutableStateOf(emptyList<StorageMediaItem>())
        private set
    var busy by mutableStateOf(false)
        private set
    var error by mutableStateOf<String?>(null)
        private set
    var mediaError by mutableStateOf(false)
        private set
    val snackbars = SnackbarHostState()
    private var refreshPending = false

    init {
        viewModelScope.launch { source.changes.collect { refresh() } }
    }

    fun refresh(action: (suspend () -> String)? = null) {
        if (busy) {
            if (action == null) refreshPending = true
            return
        }
        busy = true
        error = null
        viewModelScope.launch {
            var actionFinished = false
            try {
                val feedback = action?.invoke()
                actionFinished = action != null
                feedback?.let { viewModelScope.launch { snackbars.showSnackbar(it) } }
                usage = source.read()
                try {
                    media = source.media()
                    mediaError = false
                } catch (e: CancellationException) { throw e }
                catch (_: Exception) { mediaError = true }
            } catch (e: CancellationException) { throw e }
            catch (_: Exception) {
                error = if (actionFinished) "Changes saved. Couldn’t refresh storage; try again."
                    else "Couldn’t finish. Please try again."
            } finally {
                busy = false
                if (refreshPending) {
                    refreshPending = false
                    refresh()
                }
            }
        }
    }
}
