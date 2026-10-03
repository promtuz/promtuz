package com.promtuz.chat.utils.media

import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.async
import kotlinx.coroutines.cancel
import kotlinx.coroutines.currentCoroutineContext
import kotlinx.coroutines.launch
import kotlinx.coroutines.supervisorScope
import kotlinx.coroutines.test.runTest
import kotlinx.coroutines.withContext
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder
import java.io.File
import java.io.IOException

class OwnedFileStagingTest {
    @get:Rule val temporary = TemporaryFolder()

    private fun privateCopy(): File = temporary.newFile().apply { writeText("private media") }

    @Test fun cancellationBeforeHandoffDeletesThePrivateCopyWithoutCallingCore() = runTest {
        val file = privateCopy()
        var stages = 0
        val discarded = mutableListOf<ULong>()
        val operation = launch {
            currentCoroutineContext().cancel()
            stageOwnedFile(file, stage = { stages++; 41uL }, discard = { discarded += it })
        }
        operation.join()

        assertTrue(operation.isCancelled)
        assertEquals(0, stages)
        assertTrue(discarded.isEmpty())
        assertFalse(file.exists())
    }

    @Test fun failedHandoffDeletesOnlyThePrivateCandidate() = runTest {
        val original = temporary.newFile("original.mp4").apply { writeText("user original") }
        val file = privateCopy()
        val failure = IOException("staging refused before acceptance")
        val discarded = mutableListOf<ULong>()

        val result = runCatching {
            stageOwnedFile(file,
                stage = { withContext(Dispatchers.IO) { throw failure } },
                discard = { discarded += it },
            )
        }

        // Coroutine stacktrace recovery can copy exceptions across dispatchers.
        assertTrue(result.exceptionOrNull() is IOException)
        assertEquals(failure.message, result.exceptionOrNull()?.message)
        assertFalse(file.exists())
        assertEquals("user original", original.readText())
        assertTrue(discarded.isEmpty())
    }

    @Test fun cancellationDuringAcceptanceDiscardsTheReturnedIdWithoutUnlinkingCoreInput() = runTest {
        val file = privateCopy()
        val enteredCore = CompletableDeferred<Unit>()
        val finishAcceptance = CompletableDeferred<Unit>()
        val discarded = mutableListOf<ULong>()
        val operation = launch {
            stageOwnedFile(file,
                // Model CoreBridge's dispatcher change while native staging is already running.
                stage = { withContext(Dispatchers.IO) {
                    enteredCore.complete(Unit)
                    finishAcceptance.await()
                    42uL
                } },
                discard = { id -> withContext(Dispatchers.IO) {
                    assertTrue("core may still be hashing its input", file.exists())
                    discarded += id
                } },
            )
        }
        enteredCore.await()
        operation.cancel()
        finishAcceptance.complete(Unit)
        operation.join()

        assertTrue(operation.isCancelled)
        assertEquals(listOf(42uL), discarded)
        assertTrue("only core knows whether another owner needs these bytes", file.exists())
    }

    @Test fun failedDiscardNeverFallsBackToDeletingAnAcceptedFile() = runTest {
        val file = privateCopy()
        val enteredCore = CompletableDeferred<Unit>()
        val finishAcceptance = CompletableDeferred<Unit>()
        val discarded = mutableListOf<ULong>()
        supervisorScope {
            val operation = async {
                stageOwnedFile(file,
                    stage = {
                        enteredCore.complete(Unit)
                        finishAcceptance.await()
                        43uL
                    },
                    discard = {
                        discarded += it
                        throw IOException("core could not release the staged item")
                    },
                )
            }
            enteredCore.await()
            operation.cancel()
            finishAcceptance.complete(Unit)
            assertTrue(runCatching { operation.await() }.isFailure)
            operation.join()
        }

        assertEquals(listOf(43uL), discarded)
        assertTrue("discard failure does not transfer file ownership back", file.exists())
    }
}
