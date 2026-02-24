use super::*;

pub(super) async fn process_channel_message(
    ctx: Arc<ChannelRuntimeContext>,
    msg: traits::ChannelMessage,
    cancellation_token: CancellationToken,
) {
    if cancellation_token.is_cancelled() {
        return;
    }

    println!(
        "  💬 [{}] from {}: {}",
        msg.channel,
        msg.sender,
        truncate_with_ellipsis(&msg.content, 80)
    );
    runtime_trace::record_event(
        "channel_message_inbound",
        Some(msg.channel.as_str()),
        None,
        None,
        None,
        None,
        None,
        serde_json::json!({
            "sender": msg.sender,
            "message_id": msg.id,
            "reply_target": msg.reply_target,
            "content_preview": truncate_with_ellipsis(&msg.content, 160),
        }),
    );

    // ── Hook: on_message_received (modifying) ────────────
    let msg = if let Some(hooks) = &ctx.hooks {
        match hooks.run_on_message_received(msg).await {
            crate::hooks::HookResult::Cancel(reason) => {
                tracing::info!(%reason, "incoming message dropped by hook");
                return;
            }
            crate::hooks::HookResult::Continue(modified) => modified,
        }
    } else {
        msg
    };

    let target_channel = ctx.channels_by_name.get(&msg.channel).cloned();
    if let Err(err) = maybe_apply_runtime_config_update(ctx.as_ref()).await {
        tracing::warn!("Failed to apply runtime config update: {err}");
    }
    if handle_runtime_command_if_needed(ctx.as_ref(), &msg, target_channel.as_ref()).await {
        return;
    }

    let history_key = conversation_history_key(&msg);
    let route = get_route_selection(ctx.as_ref(), &history_key);
    let runtime_defaults = runtime_defaults_snapshot(ctx.as_ref());
    let active_provider = match get_or_create_provider(ctx.as_ref(), &route.provider).await {
        Ok(provider) => provider,
        Err(err) => {
            let safe_err = providers::sanitize_api_error(&err.to_string());
            let message = format!(
                "⚠️ Failed to initialize provider `{}`. Please run `/models` to choose another provider.\nDetails: {safe_err}",
                route.provider
            );
            if let Some(channel) = target_channel.as_ref() {
                let _ = channel
                    .send(
                        &SendMessage::new(message, &msg.reply_target)
                            .in_thread(msg.thread_ts.clone()),
                    )
                    .await;
            }
            return;
        }
    };
    if ctx.auto_save_memory && msg.content.chars().count() >= AUTOSAVE_MIN_MESSAGE_CHARS {
        let autosave_key = conversation_memory_key(&msg);
        let _ = ctx
            .memory
            .store(
                &autosave_key,
                &msg.content,
                crate::memory::MemoryCategory::Conversation,
                None,
            )
            .await;
    }

    println!("  ⏳ Processing message...");
    let started_at = Instant::now();

    let had_prior_history = ctx
        .conversation_histories
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&history_key)
        .is_some_and(|turns| !turns.is_empty());

    // Preserve user turn before the LLM call so interrupted requests keep context.
    append_sender_turn(ctx.as_ref(), &history_key, ChatMessage::user(&msg.content));

    // Build history from per-sender conversation cache.
    let prior_turns_raw = ctx
        .conversation_histories
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&history_key)
        .cloned()
        .unwrap_or_default();
    let mut prior_turns = normalize_cached_channel_turns(prior_turns_raw);

    // Only enrich with memory context when there is no prior conversation
    // history. Follow-up turns already include context from previous messages.
    if !had_prior_history {
        let memory_context =
            build_memory_context(ctx.memory.as_ref(), &msg.content, ctx.min_relevance_score).await;
        if let Some(last_turn) = prior_turns.last_mut() {
            if last_turn.role == "user" && !memory_context.is_empty() {
                last_turn.content = format!("{memory_context}{}", msg.content);
            }
        }
    }

    let system_prompt = build_channel_system_prompt(ctx.system_prompt.as_str(), &msg.channel);
    let mut history = vec![ChatMessage::system(system_prompt)];
    history.extend(prior_turns);
    let use_streaming = target_channel
        .as_ref()
        .is_some_and(|ch| ch.supports_draft_updates());

    tracing::debug!(
        channel = %msg.channel,
        has_target_channel = target_channel.is_some(),
        use_streaming,
        supports_draft = target_channel.as_ref().map_or(false, |ch| ch.supports_draft_updates()),
        "Draft streaming decision"
    );

    let (delta_tx, delta_rx) = if use_streaming {
        let (tx, rx) = tokio::sync::mpsc::channel::<String>(64);
        (Some(tx), Some(rx))
    } else {
        (None, None)
    };

    let draft_message_id = if use_streaming {
        if let Some(channel) = target_channel.as_ref() {
            match channel
                .send_draft(
                    &SendMessage::new("...", &msg.reply_target).in_thread(msg.thread_ts.clone()),
                )
                .await
            {
                Ok(id) => id,
                Err(e) => {
                    tracing::debug!("Failed to send draft on {}: {e}", channel.name());
                    None
                }
            }
        } else {
            None
        }
    } else {
        None
    };

    let draft_updater = if let (Some(mut rx), Some(draft_id_ref), Some(channel_ref)) = (
        delta_rx,
        draft_message_id.as_deref(),
        target_channel.as_ref(),
    ) {
        let channel = Arc::clone(channel_ref);
        let reply_target = msg.reply_target.clone();
        let draft_id = draft_id_ref.to_string();
        Some(tokio::spawn(async move {
            let mut accumulated = String::new();
            while let Some(delta) = rx.recv().await {
                if delta == crate::agent::loop_::DRAFT_CLEAR_SENTINEL {
                    accumulated.clear();
                    continue;
                }
                accumulated.push_str(&delta);
                if let Err(e) = channel
                    .update_draft(&reply_target, &draft_id, &accumulated)
                    .await
                {
                    tracing::debug!("Draft update failed: {e}");
                }
            }
        }))
    } else {
        None
    };

    // React with 👀 to acknowledge the incoming message
    if let Some(channel) = target_channel.as_ref() {
        if let Err(e) = channel
            .add_reaction(&msg.reply_target, &msg.id, "\u{1F440}")
            .await
        {
            tracing::debug!("Failed to add reaction: {e}");
        }
    }

    let typing_cancellation = target_channel.as_ref().map(|_| CancellationToken::new());
    let typing_task = match (target_channel.as_ref(), typing_cancellation.as_ref()) {
        (Some(channel), Some(token)) => Some(spawn_scoped_typing_task(
            Arc::clone(channel),
            msg.reply_target.clone(),
            token.clone(),
        )),
        _ => None,
    };

    // Record history length before tool loop so we can extract tool context after.
    let history_len_before_tools = history.len();

    enum LlmExecutionResult {
        Completed(Result<Result<String, anyhow::Error>, tokio::time::error::Elapsed>),
        Cancelled,
    }

    let timeout_budget_secs =
        channel_message_timeout_budget_secs(ctx.message_timeout_secs, ctx.max_tool_iterations);
    let llm_result = tokio::select! {
        () = cancellation_token.cancelled() => LlmExecutionResult::Cancelled,
        result = tokio::time::timeout(
            Duration::from_secs(timeout_budget_secs),
            run_tool_call_loop(
                active_provider.as_ref(),
                &mut history,
                ctx.tools_registry.as_ref(),
                ctx.observer.as_ref(),
                route.provider.as_str(),
                route.model.as_str(),
                runtime_defaults.temperature,
                true,
                None,
                msg.channel.as_str(),
                &ctx.multimodal,
                ctx.max_tool_iterations,
                Some(cancellation_token.clone()),
                delta_tx,
                ctx.hooks.as_deref(),
                if msg.channel == "cli" {
                    &[]
                } else {
                    ctx.non_cli_excluded_tools.as_ref()
                },
            ),
        ) => LlmExecutionResult::Completed(result),
    };

    if let Some(handle) = draft_updater {
        let _ = handle.await;
    }

    if let Some(token) = typing_cancellation.as_ref() {
        token.cancel();
    }
    if let Some(handle) = typing_task {
        log_worker_join_result(handle.await);
    }

    let reaction_done_emoji = match &llm_result {
        LlmExecutionResult::Completed(Ok(Ok(_))) => "\u{2705}", // ✅
        _ => "\u{26A0}\u{FE0F}",                                // ⚠️
    };

    match llm_result {
        LlmExecutionResult::Cancelled => {
            tracing::info!(
                channel = %msg.channel,
                sender = %msg.sender,
                "Cancelled in-flight channel request due to newer message"
            );
            runtime_trace::record_event(
                "channel_message_cancelled",
                Some(msg.channel.as_str()),
                Some(route.provider.as_str()),
                Some(route.model.as_str()),
                None,
                Some(false),
                Some("cancelled due to newer inbound message"),
                serde_json::json!({
                    "sender": msg.sender,
                    "elapsed_ms": started_at.elapsed().as_millis(),
                }),
            );
            if let (Some(channel), Some(draft_id)) =
                (target_channel.as_ref(), draft_message_id.as_deref())
            {
                if let Err(err) = channel.cancel_draft(&msg.reply_target, draft_id).await {
                    tracing::debug!("Failed to cancel draft on {}: {err}", channel.name());
                }
            }
        }
        LlmExecutionResult::Completed(Ok(Ok(response))) => {
            // ── Hook: on_message_sending (modifying) ─────────
            let mut outbound_response = response;
            if let Some(hooks) = &ctx.hooks {
                match hooks
                    .run_on_message_sending(
                        msg.channel.clone(),
                        msg.reply_target.clone(),
                        outbound_response.clone(),
                    )
                    .await
                {
                    crate::hooks::HookResult::Cancel(reason) => {
                        tracing::info!(%reason, "outgoing message suppressed by hook");
                        return;
                    }
                    crate::hooks::HookResult::Continue((
                        hook_channel,
                        hook_recipient,
                        mut modified_content,
                    )) => {
                        if hook_channel != msg.channel || hook_recipient != msg.reply_target {
                            tracing::warn!(
                                from_channel = %msg.channel,
                                from_recipient = %msg.reply_target,
                                to_channel = %hook_channel,
                                to_recipient = %hook_recipient,
                                "on_message_sending attempted to rewrite channel routing; only content mutation is applied"
                            );
                        }

                        let modified_len = modified_content.chars().count();
                        if modified_len > CHANNEL_HOOK_MAX_OUTBOUND_CHARS {
                            tracing::warn!(
                                limit = CHANNEL_HOOK_MAX_OUTBOUND_CHARS,
                                attempted = modified_len,
                                "hook-modified outbound content exceeded limit; truncating"
                            );
                            modified_content = truncate_with_ellipsis(
                                &modified_content,
                                CHANNEL_HOOK_MAX_OUTBOUND_CHARS,
                            );
                        }

                        if modified_content != outbound_response {
                            tracing::info!(
                                channel = %msg.channel,
                                sender = %msg.sender,
                                before_len = outbound_response.chars().count(),
                                after_len = modified_content.chars().count(),
                                "outgoing message content modified by hook"
                            );
                        }

                        outbound_response = modified_content;
                    }
                }
            }

            let sanitized_response =
                sanitize_channel_response(&outbound_response, ctx.tools_registry.as_ref());
            let delivered_response = if sanitized_response.is_empty()
                && !outbound_response.trim().is_empty()
            {
                "I encountered malformed tool-call output and could not produce a safe reply. Please try again.".to_string()
            } else {
                sanitized_response
            };
            runtime_trace::record_event(
                "channel_message_outbound",
                Some(msg.channel.as_str()),
                Some(route.provider.as_str()),
                Some(route.model.as_str()),
                None,
                Some(true),
                None,
                serde_json::json!({
                    "sender": msg.sender,
                    "elapsed_ms": started_at.elapsed().as_millis(),
                    "response": scrub_credentials(&delivered_response),
                }),
            );

            // Extract condensed tool-use context from the history messages
            // added during run_tool_call_loop, so the LLM retains awareness
            // of what it did on subsequent turns.
            let tool_summary = extract_tool_context_summary(&history, history_len_before_tools);
            let history_response = if tool_summary.is_empty() || msg.channel == "telegram" {
                delivered_response.clone()
            } else {
                format!("{tool_summary}\n{delivered_response}")
            };

            append_sender_turn(
                ctx.as_ref(),
                &history_key,
                ChatMessage::assistant(&history_response),
            );
            println!(
                "  🤖 Reply ({}ms): {}",
                started_at.elapsed().as_millis(),
                truncate_with_ellipsis(&delivered_response, 80)
            );
            if let Some(channel) = target_channel.as_ref() {
                if let Some(ref draft_id) = draft_message_id {
                    if let Err(e) = channel
                        .finalize_draft(&msg.reply_target, draft_id, &delivered_response)
                        .await
                    {
                        tracing::warn!("Failed to finalize draft: {e}; sending as new message");
                        let _ = channel
                            .send(
                                &SendMessage::new(&delivered_response, &msg.reply_target)
                                    .in_thread(msg.thread_ts.clone()),
                            )
                            .await;
                    }
                } else if let Err(e) = channel
                    .send(
                        &SendMessage::new(delivered_response, &msg.reply_target)
                            .in_thread(msg.thread_ts.clone()),
                    )
                    .await
                {
                    eprintln!("  ❌ Failed to reply on {}: {e}", channel.name());
                }
            }
        }
        LlmExecutionResult::Completed(Ok(Err(e))) => {
            if crate::agent::loop_::is_tool_loop_cancelled(&e) || cancellation_token.is_cancelled()
            {
                tracing::info!(
                    channel = %msg.channel,
                    sender = %msg.sender,
                    "Cancelled in-flight channel request due to newer message"
                );
                runtime_trace::record_event(
                    "channel_message_cancelled",
                    Some(msg.channel.as_str()),
                    Some(route.provider.as_str()),
                    Some(route.model.as_str()),
                    None,
                    Some(false),
                    Some("cancelled during tool-call loop"),
                    serde_json::json!({
                        "sender": msg.sender,
                        "elapsed_ms": started_at.elapsed().as_millis(),
                    }),
                );
                if let (Some(channel), Some(draft_id)) =
                    (target_channel.as_ref(), draft_message_id.as_deref())
                {
                    if let Err(err) = channel.cancel_draft(&msg.reply_target, draft_id).await {
                        tracing::debug!("Failed to cancel draft on {}: {err}", channel.name());
                    }
                }
            } else if is_context_window_overflow_error(&e) {
                let compacted = compact_sender_history(ctx.as_ref(), &history_key);
                let error_text = if compacted {
                    "⚠️ Context window exceeded for this conversation. I compacted recent history and kept the latest context. Please resend your last message."
                } else {
                    "⚠️ Context window exceeded for this conversation. Please resend your last message."
                };
                eprintln!(
                    "  ⚠️ Context window exceeded after {}ms; sender history compacted={}",
                    started_at.elapsed().as_millis(),
                    compacted
                );
                runtime_trace::record_event(
                    "channel_message_error",
                    Some(msg.channel.as_str()),
                    Some(route.provider.as_str()),
                    Some(route.model.as_str()),
                    None,
                    Some(false),
                    Some("context window exceeded"),
                    serde_json::json!({
                        "sender": msg.sender,
                        "elapsed_ms": started_at.elapsed().as_millis(),
                        "history_compacted": compacted,
                    }),
                );
                if let Some(channel) = target_channel.as_ref() {
                    if let Some(ref draft_id) = draft_message_id {
                        let _ = channel
                            .finalize_draft(&msg.reply_target, draft_id, error_text)
                            .await;
                    } else {
                        let _ = channel
                            .send(
                                &SendMessage::new(error_text, &msg.reply_target)
                                    .in_thread(msg.thread_ts.clone()),
                            )
                            .await;
                    }
                }
            } else {
                eprintln!(
                    "  ❌ LLM error after {}ms: {e}",
                    started_at.elapsed().as_millis()
                );
                let safe_error = providers::sanitize_api_error(&e.to_string());
                runtime_trace::record_event(
                    "channel_message_error",
                    Some(msg.channel.as_str()),
                    Some(route.provider.as_str()),
                    Some(route.model.as_str()),
                    None,
                    Some(false),
                    Some(&safe_error),
                    serde_json::json!({
                        "sender": msg.sender,
                        "elapsed_ms": started_at.elapsed().as_millis(),
                    }),
                );
                let should_rollback_user_turn = e
                    .downcast_ref::<providers::ProviderCapabilityError>()
                    .is_some_and(|capability| capability.capability.eq_ignore_ascii_case("vision"));
                let rolled_back = should_rollback_user_turn
                    && rollback_orphan_user_turn(ctx.as_ref(), &history_key, &msg.content);

                if !rolled_back {
                    // Close the orphan user turn so subsequent messages don't
                    // inherit this failed request as unfinished context.
                    append_sender_turn(
                        ctx.as_ref(),
                        &history_key,
                        ChatMessage::assistant("[Task failed — not continuing this request]"),
                    );
                }
                if let Some(channel) = target_channel.as_ref() {
                    if let Some(ref draft_id) = draft_message_id {
                        let _ = channel
                            .finalize_draft(&msg.reply_target, draft_id, &format!("⚠️ Error: {e}"))
                            .await;
                    } else {
                        let _ = channel
                            .send(
                                &SendMessage::new(format!("⚠️ Error: {e}"), &msg.reply_target)
                                    .in_thread(msg.thread_ts.clone()),
                            )
                            .await;
                    }
                }
            }
        }
        LlmExecutionResult::Completed(Err(_)) => {
            let timeout_msg = format!(
                "LLM response timed out after {}s (base={}s, max_tool_iterations={})",
                timeout_budget_secs, ctx.message_timeout_secs, ctx.max_tool_iterations
            );
            runtime_trace::record_event(
                "channel_message_timeout",
                Some(msg.channel.as_str()),
                Some(route.provider.as_str()),
                Some(route.model.as_str()),
                None,
                Some(false),
                Some(&timeout_msg),
                serde_json::json!({
                    "sender": msg.sender,
                    "elapsed_ms": started_at.elapsed().as_millis(),
                }),
            );
            eprintln!(
                "  ❌ {} (elapsed: {}ms)",
                timeout_msg,
                started_at.elapsed().as_millis()
            );
            // Close the orphan user turn so subsequent messages don't
            // inherit this timed-out request as unfinished context.
            append_sender_turn(
                ctx.as_ref(),
                &history_key,
                ChatMessage::assistant("[Task timed out — not continuing this request]"),
            );
            if let Some(channel) = target_channel.as_ref() {
                let error_text =
                    "⚠️ Request timed out while waiting for the model. Please try again.";
                if let Some(ref draft_id) = draft_message_id {
                    let _ = channel
                        .finalize_draft(&msg.reply_target, draft_id, error_text)
                        .await;
                } else {
                    let _ = channel
                        .send(
                            &SendMessage::new(error_text, &msg.reply_target)
                                .in_thread(msg.thread_ts.clone()),
                        )
                        .await;
                }
            }
        }
    }

    // Swap 👀 → ✅ (or ⚠️ on error) to signal processing is complete
    if let Some(channel) = target_channel.as_ref() {
        let _ = channel
            .remove_reaction(&msg.reply_target, &msg.id, "\u{1F440}")
            .await;
        let _ = channel
            .add_reaction(&msg.reply_target, &msg.id, reaction_done_emoji)
            .await;
    }
}

pub(super) async fn run_message_dispatch_loop(
    mut rx: tokio::sync::mpsc::Receiver<traits::ChannelMessage>,
    ctx: Arc<ChannelRuntimeContext>,
    max_in_flight_messages: usize,
) {
    let semaphore = Arc::new(tokio::sync::Semaphore::new(max_in_flight_messages));
    let mut workers = tokio::task::JoinSet::new();
    let in_flight_by_sender = Arc::new(tokio::sync::Mutex::new(HashMap::<
        String,
        InFlightSenderTaskState,
    >::new()));
    let task_sequence = Arc::new(AtomicU64::new(1));

    while let Some(msg) = rx.recv().await {
        let permit = match Arc::clone(&semaphore).acquire_owned().await {
            Ok(permit) => permit,
            Err(_) => break,
        };

        let worker_ctx = Arc::clone(&ctx);
        let in_flight = Arc::clone(&in_flight_by_sender);
        let task_sequence = Arc::clone(&task_sequence);
        workers.spawn(async move {
            let _permit = permit;
            let interrupt_enabled =
                worker_ctx.interrupt_on_new_message && msg.channel == "telegram";
            let sender_scope_key = interruption_scope_key(&msg);
            let cancellation_token = CancellationToken::new();
            let completion = Arc::new(InFlightTaskCompletion::new());
            let task_id = task_sequence.fetch_add(1, Ordering::Relaxed);

            if interrupt_enabled {
                let previous = {
                    let mut active = in_flight.lock().await;
                    active.insert(
                        sender_scope_key.clone(),
                        InFlightSenderTaskState {
                            task_id,
                            cancellation: cancellation_token.clone(),
                            completion: Arc::clone(&completion),
                        },
                    )
                };

                if let Some(previous) = previous {
                    tracing::info!(
                        channel = %msg.channel,
                        sender = %msg.sender,
                        "Interrupting previous in-flight request for sender"
                    );
                    previous.cancellation.cancel();
                    previous.completion.wait().await;
                }
            }

            process_channel_message(worker_ctx, msg, cancellation_token).await;

            if interrupt_enabled {
                let mut active = in_flight.lock().await;
                if active
                    .get(&sender_scope_key)
                    .is_some_and(|state| state.task_id == task_id)
                {
                    active.remove(&sender_scope_key);
                }
            }

            completion.mark_done();
        });

        while let Some(result) = workers.try_join_next() {
            log_worker_join_result(result);
        }
    }

    while let Some(result) = workers.join_next().await {
        log_worker_join_result(result);
    }
}
