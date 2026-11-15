package com.promtuz.chat.ui.screens

import androidx.activity.compose.LocalActivity
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Text
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import com.promtuz.chat.R
import com.promtuz.chat.data.ChatPrefs
import com.promtuz.chat.navigation.AppNavigator
import com.promtuz.chat.navigation.Routes
import com.promtuz.chat.security.ScreenSecurity
import com.promtuz.chat.ui.components.*
import com.promtuz.chat.utils.extensions.toHex
import com.promtuz.core.CoreBridge
import com.promtuz.core.observeQuery
import kotlinx.coroutines.launch
import uniffi.core.BlockedPerson

@Composable
fun PrivacySettingsScreen(nav: AppNavigator) {
    val activity = LocalActivity.current
    val scope = rememberCoroutineScope()
    var everyone by remember { mutableStateOf<Boolean?>(null) }
    LaunchedEffect(Unit) { everyone = runCatching { CoreBridge.messageRequestsEnabled() }.getOrDefault(true) }
    val locked by remember {
        observeQuery(setOf("messages", "conversations", "private_conversations")) { CoreBridge.privateConversationIds() }
    }.collectAsState(emptyList())
    val blocked by remember { observeQuery(setOf("prefs")) { CoreBridge.blockedPeople() } }.collectAsState(emptyList())
    var secure by remember { mutableStateOf(ChatPrefs.blockScreenshots) }

    GroupedScreen({ Text("Privacy and security") }) { padding ->
        Column(
            Modifier.fillMaxSize().verticalScroll(rememberScrollState()).padding(padding.listPadding(top = 6.dp, bottom = 24.dp)),
        ) {
            SettingsCard {
                everyone?.let { current ->
                    SettingsChoiceRow(
                        "Who can message me",
                        choices = listOf(
                            Choice(true, "Everyone", "People you haven’t added land in Message requests"),
                            Choice(false, "My contacts", "Only people you’ve added can reach you"),
                        ),
                        selected = current,
                        onSelect = { v ->
                            everyone = v
                            scope.launch { runCatching { CoreBridge.setMessageRequestsEnabled(v) } }
                        },
                    )
                } ?: SettingsRow("Who can message me")
            }
            CardGap()

            SettingsCard(header = "Protection") {
                if (locked.isNotEmpty()) SettingsRow(
                    "Locked chats",
                    subtitle = "Reset one if you forgot its code",
                    icon = R.drawable.oi_lock_chat,
                    value = locked.size.toString(),
                    trailing = Trailing.Chevron,
                    onClick = { nav.push(Routes.ResetLockedChats) },
                )
                SettingsSwitchRow(
                    "Block screenshots", secure,
                    {
                        secure = it
                        ChatPrefs.blockScreenshots = it
                        activity?.window?.let(ScreenSecurity::apply)
                    },
                    subtitle = if (secure) "Promtuz also looks blank in recent apps" else null,
                    icon = R.drawable.oi_eye_slash,
                )
            }

            if (blocked.isNotEmpty()) {
                CardGap()
                SettingsCard {
                    SettingsRow(
                        "Blocked people",
                        icon = R.drawable.oi_user_blocked,
                        value = blocked.size.toString(),
                        trailing = Trailing.Chevron,
                        onClick = { nav.push(Routes.BlockedPeople) },
                    )
                }
            }
        }
    }
}

@Composable
fun BlockedPeopleScreen() {
    val scope = rememberCoroutineScope()
    val blocked by remember { observeQuery(setOf("prefs")) { CoreBridge.blockedPeople() } }.collectAsState(emptyList())
    var unblocking by remember { mutableStateOf<BlockedPerson?>(null) }

    GroupedScreen({ Text("Blocked people") }) { padding ->
        Column(
            Modifier.fillMaxSize().verticalScroll(rememberScrollState()).padding(padding.listPadding(top = 6.dp, bottom = 24.dp)),
        ) {
            if (blocked.isNotEmpty()) SettingsCard(footer = "They can’t message you or send you requests.") {
                blocked.forEach { person ->
                    SettingsRow(
                        person.name,
                        leading = { Avatar(person.name, size = 40.dp, identityKey = person.ipk.toHex()) },
                        leadingSize = 40.dp,
                        value = "Unblock",
                        valueColor = com.promtuz.chat.ui.theme.Promtuz.colors.accent,
                        onClick = { unblocking = person },
                    )
                }
            }
        }
    }

    unblocking?.let { person ->
        AppAlertDialog(
            onDismissRequest = { unblocking = null },
            title = { Text("Unblock ${person.name}?") },
            text = { Text("They’ll be able to send you message requests again.") },
            confirmButton = {
                TextButton(onClick = {
                    unblocking = null
                    scope.launch { runCatching { CoreBridge.unblock(person.ipk) } }
                }) { Text("Unblock") }
            },
            dismissButton = { TextButton(onClick = { unblocking = null }) { Text("Cancel") } },
        )
    }
}
