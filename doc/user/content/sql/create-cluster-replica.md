---
title: "CREATE CLUSTER REPLICA"
description: "`CREATE CLUSTER REPLICA` provisions a new replica of a cluster."
pagerank: 50
menu:
  main:
    parent: commands
---

{{< warning >}}
`CREATE CLUSTER REPLICA` is deprecated.

We recommend migrating to a [managed
cluster](/sql/alter-cluster/#converting-unmanaged-to-managed-clusters) instead
of manually creating and dropping replicas.
{{< /warning >}}

`CREATE CLUSTER REPLICA` provisions a new replica of a [cluster](/get-started/key-concepts#clusters).

## Conceptual framework

A cluster consists of zero or more replicas. Each replica of a cluster is a pool
of compute resources that performs exactly the same computations on exactly the
same data.

Using multiple replicas of a cluster facilitates **fault tolerance**. Clusters
with multiple replicas can tolerate failures of the underlying hardware or
network. As long as one replica remains reachable, the cluster as a whole
remains available.

## Syntax

{{< diagram "create-cluster-replica.svg" >}}

Field | Use
------|-----
_cluster_name_ | The cluster you want to attach a replica to.
_replica_name_ | A name for this replica.

### Options

{{% replica-options %}}

## Details

### Size

The `SIZE` option for replicas is identical to the [`SIZE` option for
clusters](/sql/create-cluster/#size) option, except that the size applies only
to the new replica.

### Disk

{{< public-preview />}}

{{< warning >}}
**Pricing for this feature is likely to change.**

Replicas with disks currently consume credits at the same rate as
replicas without disks. In the future, replicas with disks will likely
consume credits at a faster rate than replicas without disks.
{{< /warning >}}

The `DISK` option for replicas works identically to the [`DISK` option for
clusters](/sql/create-cluster/#disk), except that the disk is attached only to
the new replica.

### Credit usage

The replica will consume credits at a rate determined by its size:

Size    | Credits per hour
--------|-----------------
3xsmall | 0.25
2xsmall | 0.5
xsmall  | 1
small   | 2
medium  | 4
large   | 8
xlarge  | 16
2xlarge | 32
3xlarge | 64
4xlarge | 128
5xlarge | 256
6xlarge | 512

Credit usage is measured at a one second granularity. Credit usage begins when a
`CREATE CLUSTER REPLICA` provisions the replica and ends when a [`DROP CLUSTER
REPLICA`] statement deprovisions the replica.

### Homogeneous vs. heterogeneous hardware provisioning

Because Materialize uses active replication, all replicas will be instructed to
do the same work, irrespective of their resource allocation.

For the most stable performance, we recommend using the same size and disk
configuration for all replicas.

However, it is possible to use different replica configurations in the same
cluster. In these cases, the replicas with less resources will likely be
continually burdened with a backlog of work. If all of the faster replicas
become unreachable, the system might experience delays in replying to requests
while the slower replicas catch up to the last known time that the faster
machines had computed.

## Example

```sql
CREATE CLUSTER REPLICA c1.r1 SIZE = 'medium';
```

## Privileges

The privileges required to execute this statement are:

- Ownership of `cluster_name`.

[AWS availability zone ID]: https://docs.aws.amazon.com/ram/latest/userguide/working-with-az-ids.html
[`DROP CLUSTER REPLICA`]: /sql/drop-cluster-replica
