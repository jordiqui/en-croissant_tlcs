import {
  events,
  type TlcsConnectionStatus,
  type TlcsGameState,
  commands,
} from "@/bindings";
import { Chessground } from "@/chessground/Chessground";
import {
  Badge,
  Button,
  Card,
  Flex,
  Grid,
  Group,
  NumberInput,
  Stack,
  Switch,
  Text,
  TextInput,
} from "@mantine/core";
import { useDisclosure } from "@mantine/hooks";
import { notifications } from "@mantine/notifications";
import {
  IconArrowBackUp,
  IconPlayerPlay,
  IconPlugConnected,
  IconRefresh,
  IconShieldX,
} from "@tabler/icons-react";
import { useEffect, useMemo, useState } from "react";

type FormState = {
  host: string;
  port: number;
  username: string;
  password: string;
  autoReconnect: boolean;
  reconnectIntervalMs: number;
};

const DEFAULT_FORM: FormState = {
  host: "10.0.0.2",
  port: 1965,
  username: "wg-user",
  password: "",
  autoReconnect: true,
  reconnectIntervalMs: 2000,
};

function formatClock(value?: bigint | null) {
  if (value == null) return "--:--";
  const totalSeconds = Number(value) / 1000;
  const minutes = Math.floor(totalSeconds / 60)
    .toString()
    .padStart(2, "0");
  const seconds = Math.floor(totalSeconds % 60)
    .toString()
    .padStart(2, "0");
  return `${minutes}:${seconds}`;
}

function statusColor(status: TlcsConnectionStatus | undefined) {
  switch (status) {
    case "Connected":
      return "green";
    case "Connecting":
      return "yellow";
    case "Error":
      return "red";
    default:
      return "gray";
  }
}

export function TlcsLivePage() {
  const [form, setForm] = useState<FormState>(DEFAULT_FORM);
  const [game, setGame] = useState<TlcsGameState | null>(null);
  const [lastRaw, setLastRaw] = useState<string | null>(null);
  const [connectionStatus, setConnectionStatus] =
    useState<TlcsConnectionStatus>();
  const [statusMessage, setStatusMessage] = useState<string | null>(null);
  const [boardFen, setBoardFen] = useState<string>("start");
  const [connecting, { open: startConnecting, close: stopConnecting }] =
    useDisclosure(false);

  useEffect(() => {
    const unlistenConnection = events.tlcsConnection.listen((event) => {
      setConnectionStatus(event.payload.status);
      setStatusMessage(event.payload.message ?? null);
      stopConnecting();
    });

    const unlistenGame = events.tlcsGame.listen((event) => {
      setGame(event.payload.state);
      setLastRaw(event.payload.raw ?? null);
      if (event.payload.state.fen) {
        setBoardFen(event.payload.state.fen);
      }
    });

    return () => {
      unlistenConnection.then((fn) => fn());
      unlistenGame.then((fn) => fn());
    };
  }, [stopConnecting]);

  const onConnect = async () => {
    startConnecting();
    const result = await commands.connectTlcs({
      host: form.host,
      port: form.port,
      username: form.username,
      password: form.password,
      autoReconnect: form.autoReconnect,
      reconnectIntervalMs: BigInt(form.reconnectIntervalMs),
    });

    if (result.status === "error") {
      notifications.show({
        color: "red",
        title: "Connection failed",
        message: result.error,
      });
      stopConnecting();
    }
  };

  const onDisconnect = async () => {
    await commands.disconnectTlcs();
  };

  const onReconnect = async () => {
    startConnecting();
    const res = await commands.reconnectTlcs();
    if (res.status === "error") {
      notifications.show({
        color: "red",
        title: "Reconnect failed",
        message: res.error,
      });
      stopConnecting();
    }
  };

  const sendAction = async (
    action: "AcceptOffer" | "OfferDraw" | "Resign" | "DeclineDraw",
  ) => {
    const res = await commands.sendTlcsAction(action);
    if (res.status === "error") {
      notifications.show({
        color: "red",
        title: "Action failed",
        message: res.error,
      });
    }
  };

  const connectionBadge = useMemo(
    () => (
      <Badge color={statusColor(connectionStatus)} variant="filled">
        {connectionStatus ?? "Disconnected"}
      </Badge>
    ),
    [connectionStatus],
  );

  return (
    <Grid>
      <Grid.Col span={{ base: 12, md: 4 }}>
        <Card shadow="sm" padding="lg" radius="md" withBorder>
          <Group justify="space-between" mb="md">
            <Text fw={600}>TLCS / WireGuard connection</Text>
            {connectionBadge}
          </Group>
          <Stack gap="sm">
            <TextInput
              label="WireGuard host"
              value={form.host}
              onChange={(event) =>
                setForm((f) => ({ ...f, host: event.currentTarget.value }))
              }
            />
            <NumberInput
              label="Port"
              value={form.port}
              onChange={(value) =>
                setForm((f) => ({ ...f, port: Number(value) }))
              }
              min={1}
              max={65535}
            />
            <TextInput
              label="Username"
              value={form.username}
              onChange={(event) =>
                setForm((f) => ({ ...f, username: event.currentTarget.value }))
              }
            />
            <TextInput
              label="Password"
              type="password"
              value={form.password}
              onChange={(event) =>
                setForm((f) => ({ ...f, password: event.currentTarget.value }))
              }
            />
            <NumberInput
              label="Reconnect interval (ms)"
              description="Automatically retry when the TLCS socket drops"
              value={form.reconnectIntervalMs}
              onChange={(value) =>
                setForm((f) => ({
                  ...f,
                  reconnectIntervalMs: Number(value ?? 0),
                }))
              }
              min={500}
            />
            <Switch
              label="Auto reconnect"
              checked={form.autoReconnect}
              onChange={(event) =>
                setForm((f) => ({
                  ...f,
                  autoReconnect: event.currentTarget.checked,
                }))
              }
            />
            <Group justify="space-between" mt="md">
              <Group gap="xs">
                <Button
                  leftSection={<IconPlayerPlay size={16} />}
                  loading={connecting}
                  onClick={onConnect}
                >
                  Connect
                </Button>
                <Button
                  variant="light"
                  color="red"
                  leftSection={<IconShieldX size={16} />}
                  onClick={onDisconnect}
                >
                  Disconnect
                </Button>
              </Group>
              <Button
                variant="default"
                leftSection={<IconRefresh size={16} />}
                onClick={onReconnect}
              >
                Reconnect
              </Button>
            </Group>
            {statusMessage ? (
              <Badge color={statusColor(connectionStatus)}>
                {statusMessage}
              </Badge>
            ) : null}
          </Stack>
        </Card>
      </Grid.Col>

      <Grid.Col span={{ base: 12, md: 8 }}>
        <Card shadow="sm" padding="lg" radius="md" withBorder>
          <Group justify="space-between" mb="sm">
            <Text fw={600}>Live game</Text>
            <Group gap="xs">
              <Button
                size="xs"
                leftSection={<IconPlugConnected size={14} />}
                variant="light"
                onClick={() => sendAction("AcceptOffer")}
                disabled={!game?.canAcceptDraw}
              >
                Accept offer
              </Button>
              <Button
                size="xs"
                variant="light"
                onClick={() => sendAction("OfferDraw")}
                disabled={!game?.canOfferDraw}
              >
                Offer draw
              </Button>
              <Button
                size="xs"
                color="red"
                leftSection={<IconArrowBackUp size={14} />}
                variant="light"
                onClick={() => sendAction("Resign")}
                disabled={!game?.canResign}
              >
                Resign
              </Button>
            </Group>
          </Group>

          <Grid align="stretch">
            <Grid.Col span={{ base: 12, md: 7 }}>
              <Chessground
                fen={boardFen === "start" ? undefined : boardFen}
                orientation="white"
                turnColor="white"
                movable={{ free: false, color: "both", dests: new Map() }}
                animation={{ duration: 200 }}
                highlight={{ lastMove: true }}
              />
            </Grid.Col>
            <Grid.Col span={{ base: 12, md: 5 }}>
              <Stack gap="xs">
                <Card withBorder padding="sm" radius="md">
                  <Stack gap={4}>
                    <Text size="sm" c="dimmed">
                      Status
                    </Text>
                    <Text fw={600}>
                      {game?.status ?? "Waiting for updates"}
                    </Text>
                  </Stack>
                </Card>
                <Card withBorder padding="sm" radius="md">
                  <Stack gap={4}>
                    <Text size="sm" c="dimmed">
                      Clocks
                    </Text>
                    <Flex justify="space-between">
                      <Text fw={600}>White</Text>
                      <Text>{formatClock(game?.whiteClockMs)}</Text>
                    </Flex>
                    <Flex justify="space-between">
                      <Text fw={600}>Black</Text>
                      <Text>{formatClock(game?.blackClockMs)}</Text>
                    </Flex>
                  </Stack>
                </Card>
                <Card withBorder padding="sm" radius="md">
                  <Stack gap={4}>
                    <Text size="sm" c="dimmed">
                      Last update
                    </Text>
                    <Text lineClamp={2}>{lastRaw ?? "No events yet"}</Text>
                  </Stack>
                </Card>
              </Stack>
            </Grid.Col>
          </Grid>
        </Card>
      </Grid.Col>
    </Grid>
  );
}
