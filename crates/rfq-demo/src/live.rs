//! The scenario runner over Aeron (feature `aeron`).
//!
//! Each scenario starts at `t0 = now + START_MARGIN` and sends each step at `t0 + at`. A `Tick`
//! step is only a wait: the node ticks itself whenever a timer is due. Each command's `seq`, which
//! is also the id of the request or quote it creates, is learned from the egress batch whose
//! `origin` is the command's `(actor, nonce)`, and recorded as the step's label for later steps.
//! Egress is read continuously, also while waiting, because the node gives up after 5 s of back
//! pressure from a subscriber that stops reading.
use std::collections::BTreeMap;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, bail, ensure};
use rfq_app::client::nonce::{Probe, probe_action, sync_nonce};
use rfq_app::{ClientCommand, EventBatch};
use rfq_domain::{AccountId, Action, Audience, Emitted, RejectReason};
use rfq_infra::aeron::rusteron_archive::{AeronHeader, AeronPublication, AeronSubscription, Handlers};
use rfq_infra::aeron::{self, Aeron, EGRESS, INGRESS, IPC, RESOURCE_TIMEOUT, Unconnected, frame_of, offer};
use rfq_infra::codec::{decode, encode};
use rfq_infra::fragment::Reassembler;
use rfq_testkit::accounts::{ARBITER, OPERATOR};
use rfq_testkit::rejection;
use rfq_testkit::scenarios::{Ctx, Scenario, StepKind, all, contains_in_order};

use crate::scope::{Scope, remap};

/// Lead time, in ms, between computing a scenario's `t0` and its first step.
const START_MARGIN: u64 = 500;
/// How long the node may take to see this client (it may still be starting or replaying).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(60);
/// How long a command's batch may take to come back on egress.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(5);
/// After a scenario's last step, how long its timer-driven events may take to arrive.
const GRACE: Duration = Duration::from_secs(3);
/// How long an ingress offer retries back pressure or a node that is not connected.
const OFFER_PATIENCE: Duration = Duration::from_secs(5);
/// Sleep after a poll that read nothing.
const IDLE: Duration = Duration::from_micros(200);
/// Most fragments per egress poll.
const FRAGMENTS: usize = 256;

pub struct Summary {
    passed: usize,
    total: usize,
}

impl Summary {
    pub fn all_passed(&self) -> bool {
        self.total > 0 && self.passed == self.total
    }
}

/// Runs every scenario (or only the one named `only`) against the node.
pub fn run(only: Option<&str>) -> anyhow::Result<Summary> {
    let scenarios: Vec<Scenario> = all()
        .into_iter()
        .filter(|s| only.is_none_or(|o| o == s.name))
        .collect();
    if scenarios.is_empty() {
        let names: Vec<&str> = all().iter().map(|s| s.name).collect();
        bail!(
            "no scenario named {:?}; the scenarios are {names:?}",
            only.unwrap_or_default()
        );
    }
    let dir = std::env::var("AERON_DIR").unwrap_or_else(|_| "/dev/shm/aeron".into());
    let aeron = aeron::connect(&dir).with_context(|| format!("connect to the media driver at {dir}"))?;
    let mut demo = Demo::new(Client::open(&aeron)?);
    demo.wait_for_node()?;
    for (name, role) in [("operator", OPERATOR), ("arbiter", ARBITER)] {
        demo.sync_nonce(name, role)?;
    }
    let mut passed = 0;
    for s in &scenarios {
        passed += usize::from(demo.scenario(s)?);
    }
    println!("passed {passed}/{}", scenarios.len());
    Ok(Summary {
        passed,
        total: scenarios.len(),
    })
}

/// This client's two streams: commands out on ingress, event batches in on egress.
struct Client {
    ingress: AeronPublication,
    max_message: usize,
    egress: AeronSubscription,
    /// Egress messages over the IPC MTU arrive in fragments. Egress is trusted: no size limit.
    reassembler: Reassembler,
}

impl Client {
    fn open(aeron: &Aeron) -> anyhow::Result<Self> {
        let ingress = aeron
            .async_add_publication(IPC, INGRESS)
            .and_then(|p| p.poll_blocking(RESOURCE_TIMEOUT))
            .context("add the ingress publication")?;
        let egress = aeron
            .async_add_subscription(IPC, EGRESS, Handlers::NONE, Handlers::NONE)
            .and_then(|s| s.poll_blocking(RESOURCE_TIMEOUT))
            .context("add the egress subscription")?;
        let max_message = ingress.get_constants()?.max_message_length();
        Ok(Client {
            ingress,
            max_message,
            egress,
            reassembler: Reassembler::new(usize::MAX),
        })
    }

    /// Both ends see the node: until then a command could not be offered, and the node would
    /// drop its answer (it drops egress while no subscriber is connected).
    fn connected(&self) -> bool {
        self.ingress.is_connected() && self.egress.is_connected()
    }

    fn send(&self, cmd: &ClientCommand) -> anyhow::Result<()> {
        offer(
            |bytes| self.ingress.offer(bytes),
            &encode(cmd),
            self.max_message,
            "ingress",
            Unconnected::Wait,
            OFFER_PATIENCE,
        )?;
        Ok(())
    }

    /// Polls egress once and hands every whole message to `sink`: a batch, or one chunk of a
    /// batch the node split (chunks share the batch's `seq`, `time`, `kind` and `origin`; its
    /// `balances` travel on the last chunk). Returns the number of fragments read.
    fn poll(&mut self, sink: &mut dyn FnMut(EventBatch)) -> anyhow::Result<i32> {
        ensure!(!self.egress.is_closed(), "the egress subscription is closed");
        let reassembler = &mut self.reassembler;
        let mut failure: Option<anyhow::Error> = None;
        // A panic in here would abort the process (it runs inside a C callback): record errors.
        let fragments = self.egress.poll_fn(
            |data: &[u8], header: AeronHeader| {
                if failure.is_some() {
                    return;
                }
                let (session, flags) = match frame_of(&header) {
                    Ok(frame) => frame,
                    Err(e) => {
                        failure = Some(anyhow::Error::new(e).context("read an egress fragment header"));
                        return;
                    }
                };
                reassembler.on_fragment(session, flags, data, &mut |msg| match decode(msg) {
                    Ok(batch) => sink(batch),
                    Err(e) => failure = Some(anyhow::Error::new(e).context("decode an egress batch")),
                });
            },
            FRAGMENTS,
        )?;
        match failure {
            Some(e) => Err(e),
            None => Ok(fragments),
        }
    }
}

/// What egress said about one command.
#[derive(Clone, Copy, Debug)]
struct Answer {
    seq: u64,
    rejection: Option<RejectReason>,
}

/// One scenario in progress.
struct Run {
    ctx: Ctx,
    scope: Scope,
    /// The run's own events, with the `seq` of the batch that carried each.
    own: Vec<Emitted>,
    seqs: Vec<u64>,
    /// Events of other scenarios (late timers of earlier runs) seen meanwhile.
    foreign: usize,
}

impl Run {
    fn new(t0: u64) -> Self {
        Run {
            ctx: Ctx::new(t0),
            scope: Scope::new(t0),
            own: Vec::new(),
            seqs: Vec::new(),
            foreign: 0,
        }
    }

    fn observe(&mut self, batch: &EventBatch) {
        self.ctx.observe(&batch.events);
        for e in &batch.events {
            if self.scope.owns(batch, e) {
                self.own.push(e.clone());
                self.seqs.push(batch.seq);
            } else {
                self.foreign += 1;
            }
        }
    }
}

struct Demo {
    client: Client,
    /// Each actor's last used nonce (strict nonces: the next one is `last + 1`).
    nonces: BTreeMap<AccountId, u64>,
    /// The answer to each command, by `(actor, nonce)`, until the sender takes it.
    answers: BTreeMap<(AccountId, u64), Answer>,
    run: Option<Run>,
}

impl Demo {
    fn new(client: Client) -> Self {
        Demo {
            client,
            nonces: BTreeMap::new(),
            answers: BTreeMap::new(),
            run: None,
        }
    }

    /// Reads whatever egress has. Returns the number of fragments read.
    fn pump(&mut self) -> anyhow::Result<i32> {
        let Demo {
            client, answers, run, ..
        } = self;
        client.poll(&mut |batch| {
            if let Some(origin) = batch.origin {
                let answer = answers.entry(origin).or_insert(Answer {
                    seq: batch.seq,
                    rejection: None,
                });
                // A split batch's chunks share its seq; the command's own rejection comes last.
                answer.rejection = answer.rejection.or(rejection(&batch.events));
            }
            if let Some(run) = run {
                run.observe(&batch);
            }
        })
    }

    /// Reads egress until `done` holds (`true`) or `deadline` passes (`false`).
    fn pump_until(&mut self, deadline: Instant, done: impl Fn(&Demo) -> bool) -> anyhow::Result<bool> {
        loop {
            if done(self) {
                return Ok(true);
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            if self.pump()? == 0 {
                std::thread::sleep(IDLE);
            }
        }
    }

    /// Waits until the wall clock reaches `at_ms`, reading egress throughout.
    fn wait_until_ms(&mut self, at_ms: u64) -> anyhow::Result<()> {
        while now_ms()? < at_ms {
            if self.pump()? == 0 {
                std::thread::sleep(IDLE);
            }
        }
        Ok(())
    }

    fn wait_for_node(&mut self) -> anyhow::Result<()> {
        let connected = self.pump_until(Instant::now() + CONNECT_TIMEOUT, |d| d.client.connected())?;
        ensure!(
            connected,
            "the node did not connect to this client's ingress and egress within {CONNECT_TIMEOUT:?}"
        );
        Ok(())
    }

    /// Sends `action` as `actor` with `nonce` and waits for the batch answering it; with
    /// `until_rejected`, also for the command's `CommandRejected` (every nonce probe gets one).
    fn command(
        &mut self,
        actor: AccountId,
        nonce: u64,
        action: Action,
        until_rejected: bool,
    ) -> anyhow::Result<Answer> {
        let origin = (actor, nonce);
        if let Some(run) = &mut self.run {
            run.scope.sent(origin);
        }
        self.client.send(&ClientCommand { actor, nonce, action })?;
        let answered = self.pump_until(Instant::now() + ANSWER_TIMEOUT, |d| {
            d.answers
                .get(&origin)
                .is_some_and(|a| !until_rejected || a.rejection.is_some())
        })?;
        ensure!(
            answered,
            "no answer on egress to {actor:?} nonce {nonce} within {ANSWER_TIMEOUT:?}"
        );
        self.answers.remove(&origin).context("the answer was just seen")
    }

    /// Sends `action` as `actor` with its next nonce. A valid nonce is consumed even when the
    /// action is then rejected, so the counter always moves on.
    fn send(&mut self, actor: AccountId, action: Action) -> anyhow::Result<Answer> {
        let nonce = self.nonces.get(&actor).copied().unwrap_or(0) + 1;
        self.nonces.insert(actor, nonce);
        self.command(actor, nonce, action, false)
    }

    /// Finds a genesis role's last nonce by probing (see [`rfq_app::client::nonce`]).
    fn sync_nonce(&mut self, name: &str, role: AccountId) -> anyhow::Result<()> {
        let mut probes = 0;
        let last = sync_nonce(|nonce| -> anyhow::Result<Probe> {
            probes += 1;
            let answer = self.command(role, nonce, probe_action(), true)?;
            Ok(Probe::from_rejection(answer.rejection)?)
        })
        .with_context(|| format!("find the next nonce of the {name}"))?;
        self.nonces.insert(role, last);
        println!("{name} {role:?}: next nonce {} ({probes} probe(s))", last + 1);
        Ok(())
    }

    /// Runs one scenario, prints its own events and `PASS`/`FAIL`, and returns whether it passed.
    /// A command without an answer, or a transport failure, is an error and ends the demo.
    fn scenario(&mut self, s: &Scenario) -> anyhow::Result<bool> {
        let t0 = now_ms()? + START_MARGIN;
        self.run = Some(Run::new(t0));
        println!("== {} ==", s.name);
        for step in &s.steps {
            self.wait_until_ms(t0 + step.at)?;
            // A Tick step only waits: the node sequences a Tick itself when a timer is due.
            let StepKind::Send { label, actor, action } = &step.kind else {
                continue;
            };
            let run = self.run.as_mut().context("a scenario is running")?;
            let actor = run.scope.account(*actor);
            let action = remap(action(&run.ctx), |a| run.scope.account(a));
            let answer = self
                .send(actor, action)
                .with_context(|| format!("{}: step {label}", s.name))?;
            let run = self.run.as_mut().context("a scenario is running")?;
            run.ctx.record_label(label, answer.seq);
        }
        // Wait for the events the node's timers produce after the last step.
        self.pump_until(Instant::now() + GRACE, |d| {
            d.run
                .as_ref()
                .is_some_and(|r| contains_in_order(&r.own, &s.expect).is_ok())
        })?;
        let run = self.run.take().context("a scenario is running")?;
        for (seq, e) in run.seqs.iter().zip(&run.own) {
            println!("  seq {seq:>4}  {:<22} {:?}", audience(e.audience), e.event);
        }
        if run.foreign > 0 {
            println!("  ({} events of other scenarios ignored)", run.foreign);
        }
        match contains_in_order(&run.own, &s.expect) {
            Ok(()) => {
                println!("PASS {}", s.name);
                Ok(true)
            }
            Err(e) => {
                println!("FAIL {}: {e}", s.name);
                Ok(false)
            }
        }
    }
}

fn audience(a: Audience) -> String {
    match a {
        Audience::Public => "public".into(),
        Audience::Account(id) => format!("to account {}", id.0),
    }
}

fn now_ms() -> anyhow::Result<u64> {
    let since_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before 1970")?;
    u64::try_from(since_epoch.as_millis()).context("system clock is out of range")
}
