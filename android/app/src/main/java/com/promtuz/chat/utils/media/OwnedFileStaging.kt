package com.promtuz.chat.utils.media

import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.NonCancellable
import kotlinx.coroutines.currentCoroutineContext
import kotlinx.coroutines.ensureActive
import kotlinx.coroutines.isActive
import kotlinx.coroutines.withContext
import java.io.File

/**
 * Takes one private, finished file. Before acceptance it is ours to unlink; after acceptance
 * only core may release it, since another staged item or message can share the same content.
 */
internal suspend fun stageOwnedFile(
    file: File,
    stage: suspend () -> ULong,
    discard: suspend (ULong) -> Unit,
): ULong {
    val caller = currentCoroutineContext()
    var accepted: ULong? = null
    try {
        caller.ensureActive()
        // Record acceptance before returning through CoreBridge's IO dispatcher. Cancellation
        // must never lose the id after core has accepted the path and started hashing it.
        val id = withContext(NonCancellable) { stage().also { accepted = it } }
        caller.ensureActive()
        return id
    } finally {
        val id = accepted
        if (id == null) {
            withContext(NonCancellable + Dispatchers.IO) { file.delete() }
        } else if (!caller.isActive) {
            withContext(NonCancellable) { discard(id) }
        }
    }
}
