package com.promtuz.chat.ui.components

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.Button
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.ModalBottomSheet
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Text
import androidx.compose.material3.rememberModalBottomSheetState
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableLongStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import com.promtuz.chat.presentation.state.InviteSheet
import com.promtuz.chat.navigation.Routes
import com.promtuz.chat.presentation.viewmodel.AppVM
import com.promtuz.chat.utils.extensions.toHex
import kotlinx.coroutines.delay

/**
 * Material 3 confirmation sheet for a remote invite link. Drives the pairing
 * state machine (PAIRING.md): confirm → pairing → added (PENDING) / unreachable,
 * never a false "Added" — the contact must actually reach PENDING first.
 */
@Composable
fun InviteBottomSheet(vm: AppVM) {
    val state by vm.invite.collectAsState()
    val s = state ?: return
    val sheetState = rememberModalBottomSheetState()

    ModalBottomSheet(
        onDismissRequest = vm::dismissInvite,
        sheetState = sheetState,
    ) {
        Column(
            Modifier
                .fillMaxWidth()
                .padding(horizontal = 24.dp)
                .padding(bottom = 32.dp),
            horizontalAlignment = Alignment.CenterHorizontally,
            verticalArrangement = Arrangement.spacedBy(16.dp),
        ) {
            when (s) {
                InviteSheet.Decoding -> CircularProgressIndicator()

                InviteSheet.Invalid -> Title("This invite link is invalid.")

                is InviteSheet.Pairing -> {
                    CircularProgressIndicator()
                    Title("Connecting to ${s.name}…")
                }

                is InviteSheet.Added -> {
                    Title("Added ${s.name}")
                    Subtitle("They'll confirm when they next open the app.")
                    Button(
                        onClick = {
                            vm.navigator.push(Routes.Chat(s.ipk.toHex(), s.name))
                            vm.dismissInvite()
                        },
                        Modifier.fillMaxWidth(),
                    ) { Text("Open chat") }
                }

                is InviteSheet.Unreachable -> {
                    Title("Couldn't reach ${s.name}")
                    Subtitle("They may need to open the app and connect first.")
                    Button(
                        onClick = { vm.showInvite(s.bytes) },
                        Modifier.fillMaxWidth(),
                    ) { Text("Try again") }
                    OutlinedButton(vm::dismissInvite, Modifier.fillMaxWidth()) { Text("Close") }
                }

                is InviteSheet.Confirm -> when {
                    s.isSelf -> Title("This is your own invite link.")

                    s.expiryMs <= System.currentTimeMillis() ->
                        Title("This invite expired — ask ${s.name} for a new link.")

                    s.alreadyContact -> {
                        Title("You're already connected with ${s.name}")
                        Button(
                            onClick = {
                                vm.navigator.push(Routes.Chat(s.ipk.toHex(), s.name))
                                vm.dismissInvite()
                            },
                            Modifier.fillMaxWidth(),
                        ) { Text("Open chat") }
                    }

                    else -> {
                        Text(
                            s.name,
                            style = MaterialTheme.typography.headlineSmall,
                            textAlign = TextAlign.Center,
                        )
                        Subtitle("wants to connect")
                        ExpiryCountdown(s.expiryMs)
                        Button(
                            onClick = { vm.acceptInvite(s.bytes, s.ipk, s.name) },
                            Modifier.fillMaxWidth(),
                        ) { Text("Add") }
                        OutlinedButton(vm::dismissInvite, Modifier.fillMaxWidth()) { Text("Not now") }
                    }
                }
            }
        }
    }
}

@Composable
private fun Title(text: String) = Text(
    text,
    style = MaterialTheme.typography.titleMedium,
    textAlign = TextAlign.Center,
)

@Composable
private fun Subtitle(text: String) = Text(
    text,
    style = MaterialTheme.typography.bodyMedium,
    color = MaterialTheme.colorScheme.onSurfaceVariant,
    textAlign = TextAlign.Center,
)

/** Ticks the remaining invite window each second. */
@Composable
private fun ExpiryCountdown(expiryMs: Long) {
    var now by remember { mutableLongStateOf(System.currentTimeMillis()) }
    LaunchedEffect(expiryMs) {
        while (now < expiryMs) {
            delay(1000)
            now = System.currentTimeMillis()
        }
    }
    val secs = ((expiryMs - now) / 1000).coerceAtLeast(0)
    Subtitle("Expires in ${secs / 60}:${(secs % 60).toString().padStart(2, '0')}")
}
