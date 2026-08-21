import { DEFAULT_ROUTINE_CATEGORIES } from "@sunrise/models";
import { createFileRoute, useRouter } from "@tanstack/react-router";
import {
    AlertCircle,
    Battery,
    BatteryLow,
    Calendar,
    CheckCircle,
    ChevronDown,
    ChevronRight,
    Clock,
    Edit2,
    Eye,
    EyeOff,
    Plus,
    Settings,
    Trash2,
    Zap,
} from "lucide-react";
import { useEffect, useMemo, useState } from "react";
import {
    ConflictResolution,
    type DependencyRelationship,
    EnergyLevel,
    Frequency,
    type GetRoutinesQuery,
    PriorityLevel,
    TimeOfDay,
    useCreateRoutineMutation,
    useDeleteRoutineMutation,
    useGetRoutinesQuery,
    useOnRoutineCreatedSubscription,
    useOnRoutineDeletedSubscription,
    useOnRoutineUpdatedSubscription,
    useUpdateRoutineMutation,
} from "../generated/graphql";

export const Route = createFileRoute("/routines")({
    component: RoutinesPage,
});

type GqlRoutine = GetRoutinesQuery["routines"][0];

// Create a lookup map for categories
const CATEGORY_LOOKUP = new Map(
    DEFAULT_ROUTINE_CATEGORIES.map((category) => [category.id, category]),
);

const getCategoryName = (categoryId: string): string =>
    CATEGORY_LOOKUP.get(categoryId)?.name || "Unknown Category";

const getCategoryColor = (categoryId: string): string =>
    CATEGORY_LOOKUP.get(categoryId)?.color || "#6B7280";

function getPriorityColor(priority: PriorityLevel): string {
    switch (priority) {
        case PriorityLevel.High:
            return "text-red-600 bg-red-50 border-red-200";
        case PriorityLevel.Low:
            return "text-green-600 bg-green-50 border-green-200";
        default:
            return "text-yellow-600 bg-yellow-50 border-yellow-200";
    }
}

function getEnergyIcon(level: EnergyLevel) {
    switch (level) {
        case EnergyLevel.High:
            return <Zap className="w-4 h-4 text-red-500" />;
        case EnergyLevel.Low:
            return <BatteryLow className="w-4 h-4 text-green-500" />;
        default:
            return <Battery className="w-4 h-4 text-yellow-500" />;
    }
}

function formatTimeWindow(w: {
    startHour: number;
    startMinute: number;
    endHour: number;
    endMinute: number;
}) {
    const fmt = (h: number, m = 0) => {
        const period = h >= 12 ? "PM" : "AM";
        const dh = h === 0 ? 12 : h > 12 ? h - 12 : h;
        return `${dh}${m ? `:${m.toString().padStart(2, "0")}` : ""}${period}`;
    };
    return `${fmt(w.startHour, w.startMinute)} - ${fmt(w.endHour, w.endMinute)}`;
}

const DEFAULT_CREATE_INPUT = {
    name: "",
    description: "",
    duration: { minutes: 30, flexible: false },
    priority: PriorityLevel.Medium,
    flexibility: 50,
    energyLevelRequired: EnergyLevel.Medium,
    category: DEFAULT_ROUTINE_CATEGORIES[0]?.id || "",
    frequency: Frequency.Daily,
    timePreferences: [TimeOfDay.Morning] as TimeOfDay[],
    availabilityWindows: [
        { startHour: 9, startMinute: 0, endHour: 17, endMinute: 0 },
    ],
    dependencies: [] as {
        routineId: string;
        relationship: DependencyRelationship;
        bufferMinutes?: number | null;
    }[],
    minimumGapMinutes: 0,
    bufferTimeMinutes: 5,
    conflictResolution: ConflictResolution.Reschedule,
    canBeGrouped: true,
    enabled: true,
    tags: [] as string[],
};

function RoutinesPage() {
    const router = useRouter();
    const { data, loading, error, refetch } = useGetRoutinesQuery();
    const [updateRoutine] = useUpdateRoutineMutation();
    const [deleteRoutineMutation, { loading: deletingRoutine }] =
        useDeleteRoutineMutation();
    const [createRoutineMutation] = useCreateRoutineMutation();

    // Keep the list in sync with changes made elsewhere (other clients).
    // Skip until the routines query has returned, so we never open
    // subscriptions before the session is known to be authenticated.
    const subscriptionsReady = data !== undefined;
    useOnRoutineCreatedSubscription({
        skip: !subscriptionsReady,
        onData: () => refetch(),
    });
    useOnRoutineUpdatedSubscription({
        skip: !subscriptionsReady,
        onData: () => refetch(),
    });
    useOnRoutineDeletedSubscription({
        skip: !subscriptionsReady,
        onData: () => refetch(),
    });

    // Redirect to auth when unauthenticated, like schedule.tsx does.
    useEffect(() => {
        const isUnauthenticated = error?.graphQLErrors.some(
            (gqlError) => gqlError.extensions?.code === "UNAUTHENTICATED",
        );
        if (isUnauthenticated) {
            router.navigate({ to: "/auth" });
        }
    }, [error, router]);

    const routines = data?.routines ?? [];

    const [selectedRoutineId, setSelectedRoutineId] = useState<string | null>(
        null,
    );
    const selectedRoutine = useMemo(
        () => routines.find((r) => r.id === selectedRoutineId) ?? null,
        [selectedRoutineId, routines],
    );

    // Id of the routine the edit form was seeded from. The form is only
    // shown while it matches the selected routine, so selecting another
    // routine never shows stale edit fields.
    const [editingRoutineId, setEditingRoutineId] = useState<string | null>(
        null,
    );
    const [editName, setEditName] = useState("");
    const [editDescription, setEditDescription] = useState("");
    const [editDurationMinutes, setEditDurationMinutes] = useState(30);
    const [editPriority, setEditPriority] = useState<PriorityLevel>(
        PriorityLevel.Medium,
    );
    const [editFlexibility, setEditFlexibility] = useState(50);
    const editMode =
        editingRoutineId !== null && editingRoutineId === selectedRoutineId;

    // Discard stale edit state whenever the selection moves to another
    // routine (opening an edit re-seeds the fields via openEdit).
    useEffect(() => {
        setEditingRoutineId((current) =>
            current !== null && current !== selectedRoutineId ? null : current,
        );
    }, [selectedRoutineId]);

    const [showCreateModal, setShowCreateModal] = useState(false);
    const [createInput, setCreateInput] = useState({ ...DEFAULT_CREATE_INPUT });
    const [deletingRoutineId, setDeletingRoutineId] = useState<string | null>(
        null,
    );

    // Inline error shown in the detail panel while editing.
    const [editError, setEditError] = useState<string | null>(null);
    // Inline error shown in the create modal.
    const [createError, setCreateError] = useState<string | null>(null);
    // Dismissible banner for toggle/delete failures.
    const [bannerError, setBannerError] = useState<string | null>(null);

    const [expandedCategories, setExpandedCategories] = useState(
        new Set([
            "f47ac10b-58cc-4372-a567-0e02b2c3d479",
            "550e8400-e29b-41d4-a716-446655440000",
            "6ba7b810-9dad-11d1-80b4-00c04fd430c8",
        ]),
    );

    const routinesByCategory: Map<string, GqlRoutine[]> = useMemo(() => {
        return routines.reduce((acc, routine) => {
            const key = routine.category;
            if (!acc.has(key)) acc.set(key, []);
            acc.get(key)?.push(routine);
            return acc;
        }, new Map<string, GqlRoutine[]>());
    }, [routines]);

    const stats = useMemo(() => {
        const enabled = routines.filter((r) => r.enabled);
        const totalTime = enabled.reduce(
            (sum, r) => sum + r.duration.minutes,
            0,
        );
        const highPriority = enabled.filter(
            (r) => r.priority === PriorityLevel.High,
        ).length;
        const categories = new Set(enabled.map((r) => r.category)).size;
        return {
            totalRoutines: enabled.length,
            totalTime: Math.round((totalTime / 60) * 10) / 10,
            highPriority,
            categories,
        };
    }, [routines]);

    const toggleCategory = (categoryId: string) => {
        setExpandedCategories((prev) => {
            const next = new Set(prev);
            if (next.has(categoryId)) next.delete(categoryId);
            else next.add(categoryId);
            return next;
        });
    };

    const mutationErrorMessage = (err: unknown, fallback: string) =>
        err instanceof Error && err.message ? err.message : fallback;

    // Client-side validation shared by create and edit.
    const validateRoutineFields = (fields: {
        name: string;
        durationMinutes: number;
        flexibility: number;
    }): string | null => {
        if (!fields.name.trim()) {
            return "Name is required.";
        }
        if (
            !Number.isFinite(fields.durationMinutes) ||
            fields.durationMinutes < 5 ||
            fields.durationMinutes > 480
        ) {
            return "Duration must be between 5 and 480 minutes.";
        }
        if (
            !Number.isFinite(fields.flexibility) ||
            fields.flexibility < 0 ||
            fields.flexibility > 100
        ) {
            return "Flexibility must be between 0 and 100.";
        }
        return null;
    };

    const toggleRoutineEnabled = async (routine: GqlRoutine) => {
        setBannerError(null);
        try {
            await updateRoutine({
                variables: {
                    id: routine.id,
                    input: { enabled: !routine.enabled },
                },
            });
        } catch (err) {
            setBannerError(
                mutationErrorMessage(
                    err,
                    `Failed to ${routine.enabled ? "disable" : "enable"} "${routine.name}".`,
                ),
            );
            return;
        }
        refetch();
    };

    const handleDeleteRoutine = async (routineId: string) => {
        setBannerError(null);
        try {
            await deleteRoutineMutation({ variables: { id: routineId } });
        } catch (err) {
            setDeletingRoutineId(null);
            setBannerError(
                mutationErrorMessage(err, "Failed to delete the routine."),
            );
            return;
        }
        if (selectedRoutineId === routineId) {
            setSelectedRoutineId(null);
            setEditingRoutineId(null);
        }
        setDeletingRoutineId(null);
        refetch();
    };

    const openEdit = (routine: GqlRoutine) => {
        setEditName(routine.name);
        setEditDescription(routine.description ?? "");
        setEditDurationMinutes(routine.duration.minutes);
        setEditPriority(routine.priority);
        setEditFlexibility(routine.flexibility);
        setEditError(null);
        setEditingRoutineId(routine.id);
    };

    const handleSaveEdit = async () => {
        if (!selectedRoutine) return;
        setEditError(null);
        const validationError = validateRoutineFields({
            name: editName,
            durationMinutes: editDurationMinutes,
            flexibility: editFlexibility,
        });
        if (validationError) {
            setEditError(validationError);
            return;
        }
        try {
            await updateRoutine({
                variables: {
                    id: selectedRoutine.id,
                    input: {
                        name: editName.trim(),
                        description: editDescription || null,
                        duration: {
                            minutes: editDurationMinutes,
                            flexible: selectedRoutine.duration.flexible,
                            minDuration: selectedRoutine.duration.minDuration,
                            maxDuration: selectedRoutine.duration.maxDuration,
                        },
                        priority: editPriority,
                        flexibility: editFlexibility,
                    },
                },
            });
        } catch (err) {
            setEditError(
                mutationErrorMessage(err, "Failed to save the routine."),
            );
            return;
        }
        setEditingRoutineId(null);
        refetch();
    };

    const handleCreate = async () => {
        setCreateError(null);
        const validationError = validateRoutineFields({
            name: createInput.name,
            durationMinutes: createInput.duration.minutes,
            flexibility: createInput.flexibility,
        });
        if (validationError) {
            setCreateError(validationError);
            return;
        }
        try {
            await createRoutineMutation({
                variables: {
                    input: {
                        name: createInput.name.trim(),
                        description: createInput.description || null,
                        duration: createInput.duration,
                        priority: createInput.priority,
                        flexibility: createInput.flexibility,
                        energyLevelRequired: createInput.energyLevelRequired,
                        category: createInput.category,
                        frequency: createInput.frequency,
                        timePreferences: createInput.timePreferences,
                        availabilityWindows: createInput.availabilityWindows,
                        dependencies: createInput.dependencies,
                        minimumGapMinutes: createInput.minimumGapMinutes,
                        bufferTimeMinutes: createInput.bufferTimeMinutes,
                        conflictResolution: createInput.conflictResolution,
                        canBeGrouped: createInput.canBeGrouped,
                        enabled: createInput.enabled,
                        tags: createInput.tags,
                    },
                },
            });
        } catch (err) {
            setCreateError(
                mutationErrorMessage(err, "Failed to create the routine."),
            );
            return;
        }
        setShowCreateModal(false);
        setCreateInput({ ...DEFAULT_CREATE_INPUT });
        refetch();
    };

    if (loading) {
        return (
            <div className="max-w-7xl mx-auto p-6">
                <div className="text-center py-12 text-gray-500">
                    Loading routines...
                </div>
            </div>
        );
    }

    if (error) {
        return (
            <div className="max-w-7xl mx-auto p-6">
                <div className="text-center py-12 text-red-500">
                    Failed to load routines: {error.message}
                </div>
            </div>
        );
    }

    return (
        <div className="max-w-7xl mx-auto p-6">
            {/* Toggle/delete failure banner */}
            {bannerError && (
                <div className="mb-4 flex items-center justify-between bg-red-50 border border-red-200 text-red-700 text-sm px-4 py-2 rounded-lg">
                    <span>{bannerError}</span>
                    <button
                        type="button"
                        onClick={() => setBannerError(null)}
                        className="ml-4 font-medium text-red-700 hover:text-red-900"
                        aria-label="Dismiss error"
                    >
                        Dismiss
                    </button>
                </div>
            )}

            {/* Header */}
            <div className="mb-8">
                <div className="flex items-center justify-between mb-4">
                    <div>
                        <h1 className="text-3xl font-bold text-gray-900 mb-2">
                            Routine Configuration
                        </h1>
                        <p className="text-gray-600">
                            Manage your daily routines and automation
                            preferences
                        </p>
                    </div>
                    <div className="flex items-center gap-3">
                        <button
                            type="button"
                            onClick={() => setShowCreateModal(true)}
                            className="px-4 py-2 bg-indigo-600 text-white rounded-lg hover:bg-indigo-700 transition-colors flex items-center gap-2"
                        >
                            <Plus className="w-4 h-4" />
                            Add Routine
                        </button>
                    </div>
                </div>

                {/* Stats Overview */}
                <div className="grid grid-cols-4 gap-4 mb-6">
                    <div className="bg-white rounded-xl p-4 border border-gray-200 shadow-sm">
                        <div className="flex items-center justify-between">
                            <div>
                                <p className="text-sm text-gray-600">
                                    Active Routines
                                </p>
                                <p className="text-2xl font-bold text-gray-900">
                                    {stats.totalRoutines}
                                </p>
                            </div>
                            <CheckCircle className="w-8 h-8 text-green-500" />
                        </div>
                    </div>
                    <div className="bg-white rounded-xl p-4 border border-gray-200 shadow-sm">
                        <div className="flex items-center justify-between">
                            <div>
                                <p className="text-sm text-gray-600">
                                    Daily Time
                                </p>
                                <p className="text-2xl font-bold text-gray-900">
                                    {stats.totalTime}h
                                </p>
                            </div>
                            <Clock className="w-8 h-8 text-blue-500" />
                        </div>
                    </div>
                    <div className="bg-white rounded-xl p-4 border border-gray-200 shadow-sm">
                        <div className="flex items-center justify-between">
                            <div>
                                <p className="text-sm text-gray-600">
                                    High Priority
                                </p>
                                <p className="text-2xl font-bold text-gray-900">
                                    {stats.highPriority}
                                </p>
                            </div>
                            <AlertCircle className="w-8 h-8 text-red-500" />
                        </div>
                    </div>
                    <div className="bg-white rounded-xl p-4 border border-gray-200 shadow-sm">
                        <div className="flex items-center justify-between">
                            <div>
                                <p className="text-sm text-gray-600">
                                    Categories
                                </p>
                                <p className="text-2xl font-bold text-gray-900">
                                    {stats.categories}
                                </p>
                            </div>
                            <Settings className="w-8 h-8 text-purple-500" />
                        </div>
                    </div>
                </div>
            </div>

            <div className="grid grid-cols-3 gap-6">
                {/* Routines List */}
                <div className="col-span-2 space-y-4">
                    {routines.length === 0 && (
                        <div className="bg-white rounded-xl border border-gray-200 shadow-sm p-8 text-center text-gray-500">
                            <Calendar className="w-12 h-12 mx-auto mb-4 text-gray-300" />
                            <p>
                                No routines yet. Click "Add Routine" to get
                                started.
                            </p>
                        </div>
                    )}
                    {Array.from(routinesByCategory.entries()).map(
                        ([categoryId, categoryRoutines]) => {
                            const categoryName = getCategoryName(categoryId);
                            const categoryColor = getCategoryColor(categoryId);

                            return (
                                <div
                                    key={categoryId}
                                    className="bg-white rounded-xl border border-gray-200 shadow-sm overflow-hidden"
                                >
                                    <button
                                        type="button"
                                        onClick={() =>
                                            toggleCategory(categoryId)
                                        }
                                        className="w-full px-6 py-4 flex items-center justify-between bg-gray-50 hover:bg-gray-100 transition-colors"
                                    >
                                        <div className="flex items-center gap-3 w-full">
                                            {expandedCategories.has(
                                                categoryId,
                                            ) ? (
                                                <ChevronDown className="w-5 h-5 text-gray-400" />
                                            ) : (
                                                <ChevronRight className="w-5 h-5 text-gray-400" />
                                            )}
                                            <div
                                                className="w-3 h-3 rounded-full"
                                                style={{
                                                    backgroundColor:
                                                        categoryColor,
                                                }}
                                            />
                                            <h3 className="font-semibold text-gray-900">
                                                {categoryName}
                                            </h3>
                                            <span className="px-2 py-1 bg-gray-200 text-gray-700 text-sm rounded-full">
                                                {
                                                    categoryRoutines.filter(
                                                        (r) => r.enabled,
                                                    ).length
                                                }
                                                /{categoryRoutines.length}
                                            </span>
                                        </div>
                                    </button>

                                    {expandedCategories.has(categoryId) && (
                                        <div className="divide-y divide-gray-100">
                                            {categoryRoutines.map((routine) => (
                                                // biome-ignore lint/a11y/useSemanticElements: the row contains nested action buttons, so it cannot be a native <button>
                                                <div
                                                    key={routine.id}
                                                    role="button"
                                                    tabIndex={0}
                                                    className={`w-full text-left p-4 hover:bg-gray-50 transition-colors cursor-pointer ${
                                                        selectedRoutineId ===
                                                        routine.id
                                                            ? "bg-indigo-50 border-l-4 border-indigo-500"
                                                            : ""
                                                    } ${!routine.enabled ? "opacity-60" : ""}`}
                                                    onClick={() =>
                                                        setSelectedRoutineId(
                                                            routine.id,
                                                        )
                                                    }
                                                    onKeyDown={(e) => {
                                                        if (
                                                            e.key === "Enter" ||
                                                            e.key === " "
                                                        ) {
                                                            e.preventDefault();
                                                            setSelectedRoutineId(
                                                                routine.id,
                                                            );
                                                        }
                                                    }}
                                                >
                                                    <div className="flex items-center justify-between">
                                                        <div className="flex-1">
                                                            <div className="flex items-center gap-3 mb-2">
                                                                <h4 className="font-medium text-gray-900">
                                                                    {
                                                                        routine.name
                                                                    }
                                                                </h4>
                                                                <span
                                                                    className={`px-2 py-1 text-xs rounded-full border ${getPriorityColor(routine.priority)}`}
                                                                >
                                                                    {routine.priority.toLowerCase()}
                                                                </span>
                                                                {getEnergyIcon(
                                                                    routine.energyLevelRequired,
                                                                )}
                                                            </div>
                                                            <p className="text-sm text-gray-600 mb-2">
                                                                {
                                                                    routine.description
                                                                }
                                                            </p>
                                                            <div className="flex items-center gap-4 text-xs text-gray-500">
                                                                <span className="flex items-center gap-1">
                                                                    <Clock className="w-3 h-3" />
                                                                    {routine
                                                                        .duration
                                                                        .flexible
                                                                        ? `${routine.duration.minDuration}-${routine.duration.maxDuration}min`
                                                                        : `${routine.duration.minutes}min`}
                                                                </span>
                                                                <span>
                                                                    {routine.frequency.toLowerCase()}
                                                                </span>
                                                                <span>
                                                                    Flexibility:{" "}
                                                                    {
                                                                        routine.flexibility
                                                                    }
                                                                    /100
                                                                </span>
                                                            </div>
                                                        </div>
                                                        <div className="flex items-center gap-2 ml-4">
                                                            <button
                                                                type="button"
                                                                onClick={(
                                                                    e,
                                                                ) => {
                                                                    e.stopPropagation();
                                                                    toggleRoutineEnabled(
                                                                        routine,
                                                                    );
                                                                }}
                                                                className="p-1 hover:bg-gray-200 rounded"
                                                            >
                                                                {routine.enabled ? (
                                                                    <Eye className="w-4 h-4 text-green-600" />
                                                                ) : (
                                                                    <EyeOff className="w-4 h-4 text-gray-400" />
                                                                )}
                                                            </button>
                                                            <button
                                                                type="button"
                                                                onClick={(
                                                                    e,
                                                                ) => {
                                                                    e.stopPropagation();
                                                                    setSelectedRoutineId(
                                                                        routine.id,
                                                                    );
                                                                    openEdit(
                                                                        routine,
                                                                    );
                                                                }}
                                                                className="p-1 hover:bg-gray-200 rounded"
                                                            >
                                                                <Edit2 className="w-4 h-4 text-blue-600" />
                                                            </button>
                                                            <button
                                                                type="button"
                                                                onClick={(
                                                                    e,
                                                                ) => {
                                                                    e.stopPropagation();
                                                                    setDeletingRoutineId(
                                                                        routine.id,
                                                                    );
                                                                }}
                                                                className="p-1 hover:bg-gray-200 rounded"
                                                            >
                                                                <Trash2 className="w-4 h-4 text-red-600" />
                                                            </button>
                                                        </div>
                                                    </div>
                                                </div>
                                            ))}
                                        </div>
                                    )}
                                </div>
                            );
                        },
                    )}
                </div>

                {/* Detail Panel */}
                <div className="bg-white rounded-xl border border-gray-200 shadow-sm">
                    {selectedRoutine ? (
                        <div className="p-6">
                            <div className="flex items-center justify-between mb-4">
                                <h3 className="text-lg font-semibold text-gray-900">
                                    {editMode
                                        ? "Edit Routine"
                                        : "Routine Details"}
                                </h3>
                                <div className="flex items-center gap-2">
                                    {editMode ? (
                                        <>
                                            <button
                                                type="button"
                                                onClick={() => {
                                                    setEditingRoutineId(null);
                                                    setEditError(null);
                                                }}
                                                className="px-3 py-1 text-sm bg-gray-100 text-gray-700 rounded hover:bg-gray-200"
                                            >
                                                Cancel
                                            </button>
                                            <button
                                                type="button"
                                                onClick={handleSaveEdit}
                                                className="px-3 py-1 text-sm bg-indigo-600 text-white rounded hover:bg-indigo-700"
                                            >
                                                Save
                                            </button>
                                        </>
                                    ) : (
                                        <button
                                            type="button"
                                            onClick={() =>
                                                openEdit(selectedRoutine)
                                            }
                                            className="p-2 hover:bg-gray-100 rounded"
                                        >
                                            <Edit2 className="w-4 h-4" />
                                        </button>
                                    )}
                                </div>
                            </div>

                            {editMode ? (
                                <div className="space-y-4">
                                    {editError && (
                                        <div className="bg-red-50 border border-red-200 text-red-700 text-sm px-4 py-3 rounded-lg">
                                            {editError}
                                        </div>
                                    )}
                                    <div>
                                        <label
                                            htmlFor="editName"
                                            className="block text-sm font-medium text-gray-700 mb-1"
                                        >
                                            Name
                                        </label>
                                        <input
                                            id="editName"
                                            type="text"
                                            value={editName}
                                            onChange={(e) =>
                                                setEditName(e.target.value)
                                            }
                                            className="w-full px-3 py-2 border border-gray-300 rounded-lg focus:ring-2 focus:ring-indigo-500 focus:border-indigo-500"
                                        />
                                    </div>
                                    <div>
                                        <label
                                            htmlFor="editDescription"
                                            className="block text-sm font-medium text-gray-700 mb-1"
                                        >
                                            Description
                                        </label>
                                        <textarea
                                            id="editDescription"
                                            value={editDescription}
                                            onChange={(e) =>
                                                setEditDescription(
                                                    e.target.value,
                                                )
                                            }
                                            rows={3}
                                            className="w-full px-3 py-2 border border-gray-300 rounded-lg focus:ring-2 focus:ring-indigo-500 focus:border-indigo-500"
                                        />
                                    </div>
                                    <div className="grid grid-cols-2 gap-4">
                                        <div>
                                            <label
                                                htmlFor="editDuration"
                                                className="block text-sm font-medium text-gray-700 mb-1"
                                            >
                                                Duration (min)
                                            </label>
                                            <input
                                                id="editDuration"
                                                type="number"
                                                min={5}
                                                max={480}
                                                value={editDurationMinutes}
                                                onChange={(e) =>
                                                    setEditDurationMinutes(
                                                        Number(e.target.value),
                                                    )
                                                }
                                                className="w-full px-3 py-2 border border-gray-300 rounded-lg focus:ring-2 focus:ring-indigo-500 focus:border-indigo-500"
                                            />
                                        </div>
                                        <div>
                                            <label
                                                htmlFor="editPriority"
                                                className="block text-sm font-medium text-gray-700 mb-1"
                                            >
                                                Priority
                                            </label>
                                            <select
                                                id="editPriority"
                                                value={editPriority}
                                                onChange={(e) =>
                                                    setEditPriority(
                                                        e.target
                                                            .value as PriorityLevel,
                                                    )
                                                }
                                                className="w-full px-3 py-2 border border-gray-300 rounded-lg focus:ring-2 focus:ring-indigo-500 focus:border-indigo-500"
                                            >
                                                <option
                                                    value={PriorityLevel.Low}
                                                >
                                                    Low
                                                </option>
                                                <option
                                                    value={PriorityLevel.Medium}
                                                >
                                                    Medium
                                                </option>
                                                <option
                                                    value={PriorityLevel.High}
                                                >
                                                    High
                                                </option>
                                            </select>
                                        </div>
                                    </div>
                                    <div>
                                        <label
                                            htmlFor="editFlexibility"
                                            className="block text-sm font-medium text-gray-700 mb-1"
                                        >
                                            Flexibility ({editFlexibility}
                                            /100)
                                        </label>
                                        <input
                                            id="editFlexibility"
                                            type="range"
                                            min="0"
                                            max="100"
                                            value={editFlexibility}
                                            onChange={(e) =>
                                                setEditFlexibility(
                                                    Number(e.target.value),
                                                )
                                            }
                                            className="w-full"
                                        />
                                        <div className="flex justify-between text-xs text-gray-500 mt-1">
                                            <span>Rigid</span>
                                            <span>Very Flexible</span>
                                        </div>
                                    </div>
                                </div>
                            ) : (
                                <div className="space-y-6">
                                    <div>
                                        <h4 className="font-medium text-gray-900 mb-2">
                                            {selectedRoutine.name}
                                        </h4>
                                        <p className="text-sm text-gray-600 mb-4">
                                            {selectedRoutine.description}
                                        </p>

                                        <div className="grid grid-cols-2 gap-4 text-sm">
                                            <div>
                                                <span className="text-gray-500">
                                                    Duration:
                                                </span>
                                                <span className="ml-2 font-medium">
                                                    {selectedRoutine.duration
                                                        .flexible
                                                        ? `${selectedRoutine.duration.minDuration}-${selectedRoutine.duration.maxDuration} min`
                                                        : `${selectedRoutine.duration.minutes} min`}
                                                </span>
                                            </div>
                                            <div>
                                                <span className="text-gray-500">
                                                    Priority:
                                                </span>
                                                <span
                                                    className={`ml-2 px-2 py-1 text-xs rounded ${getPriorityColor(selectedRoutine.priority)}`}
                                                >
                                                    {selectedRoutine.priority.toLowerCase()}
                                                </span>
                                            </div>
                                            <div>
                                                <span className="text-gray-500">
                                                    Energy:
                                                </span>
                                                <span className="ml-2 flex items-center gap-1">
                                                    {getEnergyIcon(
                                                        selectedRoutine.energyLevelRequired,
                                                    )}
                                                    {selectedRoutine.energyLevelRequired.toLowerCase()}
                                                </span>
                                            </div>
                                            <div>
                                                <span className="text-gray-500">
                                                    Frequency:
                                                </span>
                                                <span className="ml-2 font-medium">
                                                    {selectedRoutine.frequency.toLowerCase()}
                                                </span>
                                            </div>
                                            <div>
                                                <span className="text-gray-500">
                                                    Category:
                                                </span>
                                                <span className="ml-2 flex items-center gap-2">
                                                    <div
                                                        className="w-3 h-3 rounded-full"
                                                        style={{
                                                            backgroundColor:
                                                                getCategoryColor(
                                                                    selectedRoutine.category,
                                                                ),
                                                        }}
                                                    />
                                                    {getCategoryName(
                                                        selectedRoutine.category,
                                                    )}
                                                </span>
                                            </div>
                                        </div>
                                    </div>

                                    <div>
                                        <h5 className="font-medium text-gray-900 mb-2">
                                            Time Preferences
                                        </h5>
                                        <div className="flex flex-wrap gap-2">
                                            {selectedRoutine.timePreferences.map(
                                                (time) => (
                                                    <span
                                                        key={time}
                                                        className="px-2 py-1 bg-blue-100 text-blue-800 text-xs rounded-full"
                                                    >
                                                        {time
                                                            .toLowerCase()
                                                            .replace("_", " ")}
                                                    </span>
                                                ),
                                            )}
                                        </div>
                                    </div>

                                    <div>
                                        <h5 className="font-medium text-gray-900 mb-2">
                                            Availability Windows
                                        </h5>
                                        <div className="space-y-1">
                                            {selectedRoutine.availabilityWindows.map(
                                                (window, index) => (
                                                    <div
                                                        key={`window-${selectedRoutine.id}-${index}`}
                                                        className="text-sm text-gray-600"
                                                    >
                                                        {formatTimeWindow(
                                                            window,
                                                        )}
                                                    </div>
                                                ),
                                            )}
                                        </div>
                                    </div>

                                    <div>
                                        <h5 className="font-medium text-gray-900 mb-2">
                                            Settings
                                        </h5>
                                        <div className="text-sm space-y-1">
                                            <div className="flex justify-between">
                                                <span className="text-gray-500">
                                                    Flexibility:
                                                </span>
                                                <span>
                                                    {
                                                        selectedRoutine.flexibility
                                                    }
                                                    /100
                                                </span>
                                            </div>
                                            <div className="flex justify-between">
                                                <span className="text-gray-500">
                                                    Buffer Time:
                                                </span>
                                                <span>
                                                    {
                                                        selectedRoutine.bufferTimeMinutes
                                                    }{" "}
                                                    min
                                                </span>
                                            </div>
                                            <div className="flex justify-between">
                                                <span className="text-gray-500">
                                                    Conflict Resolution:
                                                </span>
                                                <span className="capitalize">
                                                    {selectedRoutine.conflictResolution.toLowerCase()}
                                                </span>
                                            </div>
                                            <div className="flex justify-between">
                                                <span className="text-gray-500">
                                                    Can Group:
                                                </span>
                                                <span>
                                                    {selectedRoutine.canBeGrouped
                                                        ? "Yes"
                                                        : "No"}
                                                </span>
                                            </div>
                                        </div>
                                    </div>

                                    {selectedRoutine.tags.length > 0 && (
                                        <div>
                                            <h5 className="font-medium text-gray-900 mb-2">
                                                Tags
                                            </h5>
                                            <div className="flex flex-wrap gap-1">
                                                {selectedRoutine.tags.map(
                                                    (tag) => (
                                                        <span
                                                            key={tag}
                                                            className="px-2 py-1 bg-gray-100 text-gray-700 text-xs rounded"
                                                        >
                                                            {tag}
                                                        </span>
                                                    ),
                                                )}
                                            </div>
                                        </div>
                                    )}
                                </div>
                            )}
                        </div>
                    ) : (
                        <div className="p-6 text-center text-gray-500">
                            <Calendar className="w-12 h-12 mx-auto mb-4 text-gray-300" />
                            <p>Select a routine to view details</p>
                        </div>
                    )}
                </div>
            </div>

            {/* Create Routine Modal */}
            {showCreateModal && (
                <div className="fixed inset-0 bg-black/50 flex items-center justify-center z-50">
                    <div className="bg-white rounded-xl p-6 w-full max-w-md shadow-xl">
                        <h2 className="text-lg font-semibold mb-4">
                            New Routine
                        </h2>
                        {createError && (
                            <div className="mb-4 bg-red-50 border border-red-200 text-red-700 text-sm px-4 py-3 rounded-lg">
                                {createError}
                            </div>
                        )}
                        <div className="space-y-3">
                            <div>
                                <label
                                    htmlFor="createName"
                                    className="block text-sm font-medium text-gray-700 mb-1"
                                >
                                    Name *
                                </label>
                                <input
                                    id="createName"
                                    type="text"
                                    value={createInput.name}
                                    onChange={(e) =>
                                        setCreateInput((p) => ({
                                            ...p,
                                            name: e.target.value,
                                        }))
                                    }
                                    className="w-full px-3 py-2 border border-gray-300 rounded-lg focus:ring-2 focus:ring-indigo-500"
                                    placeholder="e.g. Morning exercise"
                                />
                            </div>
                            <div>
                                <label
                                    htmlFor="createDesc"
                                    className="block text-sm font-medium text-gray-700 mb-1"
                                >
                                    Description
                                </label>
                                <textarea
                                    id="createDesc"
                                    value={createInput.description}
                                    onChange={(e) =>
                                        setCreateInput((p) => ({
                                            ...p,
                                            description: e.target.value,
                                        }))
                                    }
                                    rows={2}
                                    className="w-full px-3 py-2 border border-gray-300 rounded-lg focus:ring-2 focus:ring-indigo-500"
                                />
                            </div>
                            <div className="grid grid-cols-2 gap-3">
                                <div>
                                    <label
                                        htmlFor="createDuration"
                                        className="block text-sm font-medium text-gray-700 mb-1"
                                    >
                                        Duration (min)
                                    </label>
                                    <input
                                        id="createDuration"
                                        type="number"
                                        min={5}
                                        max={480}
                                        value={createInput.duration.minutes}
                                        onChange={(e) =>
                                            setCreateInput((p) => ({
                                                ...p,
                                                duration: {
                                                    ...p.duration,
                                                    minutes: Number(
                                                        e.target.value,
                                                    ),
                                                },
                                            }))
                                        }
                                        className="w-full px-3 py-2 border border-gray-300 rounded-lg focus:ring-2 focus:ring-indigo-500"
                                    />
                                </div>
                                <div>
                                    <label
                                        htmlFor="createPriority"
                                        className="block text-sm font-medium text-gray-700 mb-1"
                                    >
                                        Priority
                                    </label>
                                    <select
                                        id="createPriority"
                                        value={createInput.priority}
                                        onChange={(e) =>
                                            setCreateInput((p) => ({
                                                ...p,
                                                priority: e.target
                                                    .value as PriorityLevel,
                                            }))
                                        }
                                        className="w-full px-3 py-2 border border-gray-300 rounded-lg focus:ring-2 focus:ring-indigo-500"
                                    >
                                        <option value={PriorityLevel.Low}>
                                            Low
                                        </option>
                                        <option value={PriorityLevel.Medium}>
                                            Medium
                                        </option>
                                        <option value={PriorityLevel.High}>
                                            High
                                        </option>
                                    </select>
                                </div>
                            </div>
                            <div>
                                <label
                                    htmlFor="createCategory"
                                    className="block text-sm font-medium text-gray-700 mb-1"
                                >
                                    Category
                                </label>
                                <select
                                    id="createCategory"
                                    value={createInput.category}
                                    onChange={(e) =>
                                        setCreateInput((p) => ({
                                            ...p,
                                            category: e.target.value,
                                        }))
                                    }
                                    className="w-full px-3 py-2 border border-gray-300 rounded-lg focus:ring-2 focus:ring-indigo-500"
                                >
                                    {DEFAULT_ROUTINE_CATEGORIES.map((cat) => (
                                        <option key={cat.id} value={cat.id}>
                                            {cat.name}
                                        </option>
                                    ))}
                                </select>
                            </div>
                            <div>
                                <label
                                    htmlFor="createFrequency"
                                    className="block text-sm font-medium text-gray-700 mb-1"
                                >
                                    Frequency
                                </label>
                                <select
                                    id="createFrequency"
                                    value={createInput.frequency}
                                    onChange={(e) =>
                                        setCreateInput((p) => ({
                                            ...p,
                                            frequency: e.target
                                                .value as Frequency,
                                        }))
                                    }
                                    className="w-full px-3 py-2 border border-gray-300 rounded-lg focus:ring-2 focus:ring-indigo-500"
                                >
                                    <option value={Frequency.Daily}>
                                        Daily
                                    </option>
                                    <option value={Frequency.Weekdays}>
                                        Weekdays
                                    </option>
                                    <option value={Frequency.Weekends}>
                                        Weekends
                                    </option>
                                    <option value={Frequency.Weekly}>
                                        Weekly
                                    </option>
                                    <option value={Frequency.Custom}>
                                        Custom
                                    </option>
                                </select>
                            </div>
                        </div>
                        <div className="flex justify-end gap-2 mt-6">
                            <button
                                type="button"
                                onClick={() => {
                                    setShowCreateModal(false);
                                    setCreateInput({ ...DEFAULT_CREATE_INPUT });
                                    setCreateError(null);
                                }}
                                className="px-4 py-2 text-sm bg-gray-100 text-gray-700 rounded-lg hover:bg-gray-200"
                            >
                                Cancel
                            </button>
                            <button
                                type="button"
                                onClick={handleCreate}
                                disabled={!createInput.name.trim()}
                                className="px-4 py-2 text-sm bg-indigo-600 text-white rounded-lg hover:bg-indigo-700 disabled:opacity-50"
                            >
                                Create
                            </button>
                        </div>
                    </div>
                </div>
            )}

            {/* Delete confirmation */}
            {deletingRoutineId && (
                <div className="fixed inset-0 bg-black/50 flex items-center justify-center z-50">
                    <div className="bg-white rounded-xl shadow-xl p-6 w-full max-w-sm mx-4">
                        <h2 className="text-lg font-semibold mb-2">
                            Delete Routine
                        </h2>
                        <p className="text-gray-600 text-sm mb-4">
                            Are you sure you want to delete this routine? This
                            cannot be undone.
                        </p>
                        <div className="flex gap-3 justify-end">
                            <button
                                type="button"
                                onClick={() => setDeletingRoutineId(null)}
                                disabled={deletingRoutine}
                                className="px-4 py-2 text-sm bg-gray-100 text-gray-700 rounded-lg hover:bg-gray-200 disabled:opacity-50"
                            >
                                Cancel
                            </button>
                            <button
                                type="button"
                                onClick={() =>
                                    handleDeleteRoutine(deletingRoutineId)
                                }
                                disabled={deletingRoutine}
                                className="px-4 py-2 text-sm bg-red-600 text-white rounded-lg hover:bg-red-700 disabled:opacity-50"
                            >
                                {deletingRoutine ? "Deleting..." : "Delete"}
                            </button>
                        </div>
                    </div>
                </div>
            )}
        </div>
    );
}
