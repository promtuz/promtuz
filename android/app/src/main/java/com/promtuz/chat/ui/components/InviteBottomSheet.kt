package com.promtuz.chat.ui.components

import com.promtuz.chat.ui.theme.Promtuz

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.MaterialTheme
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
import com.promtuz.chat.ui.text.clock
import com.promtuz.chat.utils.extensions.toHex
import kotlinx.coroutines.delay

/** Never shows a false "Added": the contact must actually reach PENDING first. */
@Composable
fun InviteBottomSheet(vm: AppVM) {
    val state by vm.invite.collectAsState()
    val s = state ?: return
    val sheetState = rememberModalBottomSheetState()

    AppBottomSheet(
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

                is InviteSheet.Invalid -> Title(s.message)

                is InviteSheet.Pairing -> {
                    CircularProgressIndicator()
                    Title("Connecting to ${s.name}…")
                }

                is InviteSheet.Added -> {
                    Title("Added ${s.name}")
                    Subtitle("They'll confirm when they next open the app.")
                    AppButton(
                        "Open chat",
                        onClick = {
                            vm.openChatWith(s.ipk.toHex(), s.name)
                            vm.dismissInvite()
                        },
                        Modifier.fillMaxWidth(),
                    )
                }

                is InviteSheet.Unreachable -> {
                    Title("Couldn't reach ${s.name}")
                    Subtitle("They may need to open the app and connect first.")
                    AppButton("Try again", { vm.showInvite(s.bytes) }, Modifier.fillMaxWidth())
                    AppButton("Close", vm::dismissInvite, Modifier.fillMaxWidth(), kind = ButtonKind.Tonal)
                }

                is InviteSheet.Confirm -> when {
                    s.expiryMs <= System.currentTimeMillis() ->
                        Title("This invite expired. Ask ${s.name} for a new link.")

                    s.alreadyContact -> {
                        Title("You're already connected with ${s.name}")
                        AppButton(
                            "Open chat",
                            onClick = {
                                vm.openChatWith(s.ipk.toHex(), s.name)
                                vm.dismissInvite()
                            },
                            Modifier.fillMaxWidth(),
                        )
                    }

                    else -> {
                        Text(
                            s.name,
                            style = MaterialTheme.typography.headlineSmall,
                            textAlign = TextAlign.Center,
                        )
                        Subtitle("wants to connect")
                        ExpiryCountdown(s.expiryMs)
                        AppButton("Add", { vm.acceptInvite(s.bytes, s.ipk, s.name) }, Modifier.fillMaxWidth())
                        AppButton("Not now", vm::dismissInvite, Modifier.fillMaxWidth(), kind = ButtonKind.Tonal)
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
    color = Promtuz.colors.muted,
    textAlign = TextAlign.Center,
)

@Composable
private fun ExpiryCountdown(expiryMs: Long) {
    var now by remember { mutableLongStateOf(System.currentTimeMillis()) }
    LaunchedEffect(expiryMs) {
        while (now < expiryMs) {
            delay(1000)
            now = System.currentTimeMillis()
        }
    }
    Subtitle("Expires in ${clock((expiryMs - now).coerceAtLeast(0))}")
}
